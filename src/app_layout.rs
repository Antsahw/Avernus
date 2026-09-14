use gpui::{StatefulInteractiveElement, *};
use gpui_component::button::{Button, ButtonVariant};
use gpui_component::dialog::Dialog;
use gpui_component::input::{Input, InputState};
use gpui_component::menu::PopupMenuItem;
use gpui_component::sidebar::*;
use gpui_component::{IconName, Root, *};
use std::fmt;
use gpui_component::button::ButtonVariants;
use gpui_component::scroll::ScrollableElement;
use crate::matrix::MatrixBackend;

#[derive(Clone)]
pub struct Space {
    pub id: String,
    pub name: String,
    pub icon: IconName,
}

impl PartialEq for Space {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.icon.clone().path() == other.icon.clone().path()
    }
}

impl Eq for Space {}

impl fmt::Debug for Space {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Space")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("icon", &self.icon.clone().path())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum DialogType {
    StartConversation,
    NewRoom,
    DiscoverAndJoin,
    AccountSignIn,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RoomDetailsTab {
    Members,
    Details,
}

pub struct AvernusApp {
    pub matrix: Entity<MatrixBackend>,
    pub active_sidebar: Option<String>,
    pub active_item: Option<String>,
    pub message_input: Entity<InputState>,
    pub dialog_input: Entity<InputState>,
    pub login_user: Entity<InputState>,
    pub login_password: Entity<InputState>,
    pub active_dialog: Option<DialogType>,
    pub show_room_details: bool,
    pub room_details_tab: RoomDetailsTab,
    pub joined_spaces: Vec<Space>,
    pub message_scroll_handle: ScrollHandle,
    pub last_message_count: std::collections::HashMap<String, usize>,
    pub matrix_subscription: Subscription,
    pub message_input_subscription: Subscription,
    pub discover_input_subscription: Subscription,
}

impl AvernusApp {
    fn render_verification_challenge_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(challenge) = self.matrix.read(cx).pending_verification.clone() else {
            return Dialog::new(cx)
                .title("Verify device")
                .child(div().text_sm().child("Verification challenge is unavailable."))
                .footer(
                    h_flex()
                        .justify_end()
                        .child(
                            Button::new("verification-close")
                                .label("Close")
                                .with_variant(ButtonVariant::Ghost)
                                .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                    this.matrix.update(cx, |backend, _| {
                                        backend.pending_verification = None;
                                    });
                                    cx.notify();
                                })),
                        ),
                );
        };

        let matrix_for_confirm = self.matrix.clone();
        let user_id_for_confirm = challenge.user_id.clone();
        let flow_id_for_confirm = challenge.flow_id.clone();

        let matrix_for_mismatch = self.matrix.clone();
        let user_id_for_mismatch = challenge.user_id.clone();
        let flow_id_for_mismatch = challenge.flow_id.clone();

        Dialog::new(cx)
            .title("Verify this device")
            .child(
                v_flex()
                    .w(rems(28.0))
                    .gap_3()
                    .child(div().text_sm().child("Compare the emoji pairs with the other session and respond to the exact same challenge."))
                    .child(
                        v_flex()
                            .gap_2()
                            .children(challenge.emojis.iter().enumerate().map(|(index, emoji)| {
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .gap_2()
                                    .p_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .child(div().text_xs().font_bold().child(format!("{}. ", index + 1)))
                                    .child(div().text_2xl().child(emoji.symbol.clone()))
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(emoji.description.clone()))
                                    .into_any_element()
                            })),
                    ),
            )
            .footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("verification-mismatch")
                            .label("They don't match")
                            .with_variant(ButtonVariant::Danger)
                            .on_click(cx.listener(move |this: &mut AvernusApp, _, _, cx| {
                                let matrix_entity = matrix_for_mismatch.clone();
                                let user_id = user_id_for_mismatch.clone();
                                let flow_id = flow_id_for_mismatch.clone();
                                cx.spawn(async move |_this, cx| {
                                    let _ = cx.update(|cx| {
                                        matrix_entity.update(cx, |backend, cx| {
                                            backend.mismatch_verification_challenge(cx, user_id.clone(), flow_id.clone());
                                        });
                                    });
                                })
                                .detach();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("verification-confirm")
                            .label("They match")
                            .on_click(cx.listener(move |this: &mut AvernusApp, _, _, cx| {
                                let matrix_entity = matrix_for_confirm.clone();
                                let user_id = user_id_for_confirm.clone();
                                let flow_id = flow_id_for_confirm.clone();
                                cx.spawn(async move |_this, cx| {
                                    let _ = cx.update(|cx| {
                                        matrix_entity.update(cx, |backend, cx| {
                                            backend.confirm_verification_challenge(cx, user_id.clone(), flow_id.clone());
                                        });
                                    });
                                })
                                .detach();
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_modal_dialog(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, placeholder, button_label) = match self.active_dialog {
            Some(DialogType::StartConversation) => (
                "Start Conversation",
                "Enter Matrix User ID (e.g., @user:matrix.org)...",
                "Start Chat",
            ),
            Some(DialogType::NewRoom) => ("Create New Room", "Enter room name...", "Create Room"),
            Some(DialogType::DiscoverAndJoin) => (
                "Discover / Join Room or Space",
                "Search public rooms or paste a Room ID / Alias (e.g., #room:matrix.org)...",
                "Search",
            ),
            Some(DialogType::AccountSignIn) => (
                "Sign In with Matrix",
                "Enter Homeserver URL (e.g., https://matrix.org)...",
                "Open Browser to Sign In",
            ),
            None => ("", "", ""),
        };

        self.dialog_input.update(cx, |input_state, cx| {
            input_state.set_placeholder(placeholder, window, cx);
        });

        let dialog_input = self.dialog_input.clone();
        let public_room_search_results = if self.active_dialog == Some(DialogType::DiscoverAndJoin) {
            self.matrix.read(cx).public_rooms.clone()
        } else {
            Vec::new()
        };

        let discover_elements: Vec<gpui::AnyElement> = if self.active_dialog == Some(DialogType::DiscoverAndJoin) {
            if public_room_search_results.is_empty() {
                vec![div().text_sm().text_color(cx.theme().muted_foreground).child("Search for public rooms by name, topic, or keyword.").into_any_element()]
            } else {
                public_room_search_results
                    .iter()
                    .map(|room| {
                        let room_title = room
                            .name
                            .clone()
                            .unwrap_or_else(|| room.alias.clone().unwrap_or_else(|| "Public room".to_string()));
                        let room_alias = room.alias.clone().unwrap_or_else(|| room.room_id.clone());
                        let room_id = room.room_id.clone();
                        let room_topic = room.topic.clone();
                        let matrix_entity = self.matrix.clone();

                        Button::new(format!("discover-room-{}", room_id))
                            .label(format!("{}{}", room_title, if room_topic.as_deref().is_some() { "" } else { "" }))
                            .with_variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(move |this: &mut AvernusApp, _, _, cx| {
                                let alias_or_id = room_alias.clone();
                                let matrix_entity = matrix_entity.clone();
                                cx.spawn(async move |_this, cx| {
                                    let _ = cx.update(|cx| {
                                        matrix_entity.update(cx, |backend, cx| {
                                            backend.join_room_by_id_or_alias(cx, alias_or_id.clone());
                                        });
                                    });
                                })
                                .detach();
                                this.active_dialog = None;
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .collect()
            }
        } else {
            vec![]
        };

        Dialog::new(cx)
            .title(title)
            .child(
                v_flex()
                    .gap_4()
                    .w(rems(24.0))
                    .child(Input::new(&dialog_input).cleanable(true).large())
                    .children(discover_elements),
            )
            .footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .with_variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                this.active_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("submit")
                            .label(button_label)
                            .on_click(cx.listener(|this: &mut AvernusApp, _, _window, cx| {
                                let query = this
                                    .dialog_input
                                    .read(cx)
                                    .text()
                                    .to_string();
                                let query = query.trim().to_string();
                                let dialog_type = this.active_dialog.clone();
                                let should_close_dialog = matches!(
                                    dialog_type,
                                    Some(DialogType::StartConversation)
                                        | Some(DialogType::NewRoom)
                                        | Some(DialogType::AccountSignIn)
                                );
                                if should_close_dialog {
                                    this.active_dialog = None;
                                }

                                if !query.is_empty() {
                                    let matrix_entity = this.matrix.clone();
                                    cx.spawn(async move |_this, cx| {
                                        let _ = cx.update(|cx| {
                                            match dialog_type {
                                                Some(DialogType::StartConversation) => {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.create_direct_chat(cx, query);
                                                    });
                                                }
                                                Some(DialogType::NewRoom) => {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.create_room(cx, query);
                                                    });
                                                }
                                                Some(DialogType::DiscoverAndJoin) => {
                                                    if query.starts_with('#') || query.starts_with('!') || query.starts_with("http://") || query.starts_with("https://") {
                                                        matrix_entity.update(cx, |backend, cx| {
                                                            backend.join_room_by_id_or_alias(cx, query);
                                                        });
                                                    } else {
                                                        matrix_entity.update(cx, |backend, cx| {
                                                            backend.search_public_rooms(cx, query);
                                                        });
                                                    }
                                                }
                                                Some(DialogType::AccountSignIn) => {
                                                    let homeserver = if query.starts_with("http://") || query.starts_with("https://") {
                                                        query
                                                    } else {
                                                        format!("https://{}", query)
                                                    };
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.start_sso_login(cx, homeserver);
                                                    });
                                                }
                                                None => {}
                                            }
                                        });
                                    })
                                    .detach();
                                }
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_discover_panel(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_input = self.dialog_input.clone();
        self.dialog_input.update(cx, |input_state, cx| {
            input_state.set_placeholder("Search public rooms or paste #room:server", window, cx);
        });

        let matrix_state = self.matrix.read(cx);
        let joined_room_ids: std::collections::HashSet<String> = matrix_state
            .rooms
            .iter()
            .map(|room| room.id.clone())
            .collect();
        let public_rooms = matrix_state.public_rooms.clone();

        v_flex()
            .flex_1()
            .h_full()
            .min_w(rems(22.0))
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .p_4()
            .gap_4()
            .child(div().text_lg().font_bold().child("Discover public rooms"))
            .child(Input::new(&panel_input).cleanable(true).large())
            .child(
                if public_rooms.is_empty() {
                    div().text_sm().text_color(cx.theme().muted_foreground).child("Search by room name, topic, or keyword.")
                } else {
                    v_flex()
                        .w_full()
                        .gap_2()
                        .children(public_rooms.iter().map(|room| {
                            let room_id = room.room_id.clone();
                            let room_alias = room.alias.clone().unwrap_or_else(|| room.room_id.clone());
                            let room_alias_for_display = room_alias.clone();
                            let title = room.name.clone().unwrap_or_else(|| room.alias.clone().unwrap_or_else(|| "Public room".to_string()));
                            let topic = room.topic.clone().unwrap_or_else(|| "No description".to_string());
                            let joined = joined_room_ids.contains(&room.room_id);
                            let matrix_entity = self.matrix.clone();

                            h_flex()
                                .w_full()
                                .justify_between()
                                .items_center()
                                .p_2()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().border)
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .gap_1()
                                        .child(div().text_sm().font_bold().child(title))
                                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(topic))
                                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(room_alias_for_display)),
                                )
                                .child(
                                    Button::new(format!("join-public-room-{}", room_id))
                                        .label(if joined { "Joined" } else { "Join" })
                                        .with_variant(if joined {
                                            ButtonVariant::Secondary
                                        } else {
                                            ButtonVariant::Ghost
                                        })
                                        .on_click(cx.listener(move |this: &mut AvernusApp, _, _, cx| {
                                            let alias_or_id = room_alias.clone();
                                            let matrix_entity = matrix_entity.clone();
                                            cx.spawn(async move |_this, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.join_room_by_id_or_alias(cx, alias_or_id.clone());
                                                    });
                                                });
                                            })
                                            .detach();
                                            this.active_sidebar = Some("Space: Discover".to_string());
                                            cx.notify();
                                        })),
                                )
                        }))
                },
            )
    }

    fn render_room_details_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_tab = self.room_details_tab;

        v_flex()
            .w(rems(18.0))
            .h_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .p_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("tab-members")
                                    .label("Members")
                                    .with_variant(if current_tab == RoomDetailsTab::Members {
                                        ButtonVariant::Secondary
                                    } else {
                                        ButtonVariant::Ghost
                                    })
                                    .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                        this.room_details_tab = RoomDetailsTab::Members;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("tab-details")
                                    .label("Details")
                                    .with_variant(if current_tab == RoomDetailsTab::Details {
                                        ButtonVariant::Secondary
                                    } else {
                                        ButtonVariant::Ghost
                                    })
                                    .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                        this.room_details_tab = RoomDetailsTab::Details;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        Button::new("close-details")
                            .icon(IconName::Close)
                            .with_variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                this.show_room_details = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .overflow_y_scrollbar()
                    .p_3()
                    .gap_3()
                    .child(match current_tab {
                        RoomDetailsTab::Members => v_flex()
                            .gap_2()
                            .child(div().text_sm().font_bold().child("Room Members"))
                            .child(self.render_member_item("Active User", "Member", true, cx)),
                        RoomDetailsTab::Details => v_flex().gap_3().child(
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().font_bold().child("ENCRYPTION"))
                                .child(div().text_sm().child("🔒 End-to-End Encrypted")),
                        ),
                    }),
            )
    }

    fn render_member_item(
        &self,
        name: &str,
        role: &str,
        is_online: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_color = if is_online {
            gpui::green()
        } else {
            cx.theme().muted_foreground
        };

        h_flex()
            .items_center()
            .justify_between()
            .p_2()
            .rounded_md()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().size_2().rounded_full().bg(status_color))
                    .child(div().text_sm().font_medium().child(name.to_string())),
            )
            .child(div().text_xs().child(role.to_string()))
    }
}

impl Render for AvernusApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let matrix_state = self.matrix.read(cx);

        // History loading is triggered only from explicit room selection, not during render.
        // Rendering should be side-effect free to avoid duplicate/racing async requests.

        // --- Main Section Group ---
        let main_group = SidebarGroup::new("Main")
            .child(
                SidebarMenuItem::new("Direct Messages")
                    .icon(IconName::LayoutDashboard)
                    .on_click(cx.listener(|this: &mut AvernusApp, _event, _window, cx| {
                        this.active_sidebar = Some("Direct Messages".to_string());
                        this.active_item = None;
                        cx.notify();
                    })),
            )
            .child(
                SidebarMenuItem::new("Rooms")
                    .icon(IconName::LayoutDashboard)
                    .on_click(cx.listener(|this: &mut AvernusApp, _event, _window, cx| {
                        this.active_sidebar = Some("Rooms".to_string());
                        this.active_item = None;
                        cx.notify();
                    })),
            );

        let spaces_source = self.joined_spaces.clone();

        // --- Spaces Section Group ---
        let spaces_group = SidebarGroup::new("Spaces")
            .child(
                SidebarMenuItem::new("Discover")
                    .icon(IconName::Folder)
                    .on_click(cx.listener(|this: &mut AvernusApp, _event, _window, cx| {
                        this.active_sidebar = Some("Space: Discover".to_string());
                        this.active_item = None;
                        cx.notify();
                    })),
            )
            .children(spaces_source.iter().map(|space| {
                let space_id = space.id.clone();
                let space_name = space.name.clone();
                let sidebar_key = format!("Space: {}", space_id);

                SidebarMenuItem::new(space_name)
                    .icon(space.icon.clone())
                    .on_click(
                        cx.listener(move |this: &mut AvernusApp, _event, _window, cx| {
                            this.active_sidebar = Some(sidebar_key.clone());
                            this.active_item = None;
                            cx.notify();
                        }),
                    )
            }));

        // --- Render Primary Sidebar ---
        let primary_sidebar = Sidebar::new("myapp")
            .h_full()
            .header(SidebarHeader::new().child("Avernus Matrix"))
            .child(main_group)
            .child(spaces_group)
            .footer(
                SidebarFooter::new().child(
                    Button::new("settings-btn")
                        .with_variant(ButtonVariant::Ghost)
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(Icon::new(IconName::Settings))
                                .child(div().child("Settings")),
                        )
                        .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                            this.active_sidebar = Some("Settings".to_string());
                            this.active_item = None;
                            cx.notify();
                        })),
                ),
            );

        let mut row = h_flex().size_full().child(primary_sidebar);

        // --- Render Secondary Sidebar ---
        if let Some(selection) = &self.active_sidebar {
            if selection != "Space: Discover" {
                let mut secondary_menu = SidebarMenu::new();

                if selection == "Direct Messages" {
                    secondary_menu =
                        secondary_menu.child(SidebarMenuItem::new("+ Start conversation").on_click(
                            cx.listener(|this: &mut AvernusApp, _event, _window, cx| {
                                this.active_dialog = Some(DialogType::StartConversation);
                                cx.notify();
                            }),
                        ));

                    let direct_rooms: Vec<_> = matrix_state.rooms.iter().filter(|r| r.is_direct).cloned().collect();
                    if direct_rooms.is_empty() {
                        secondary_menu = secondary_menu.child(SidebarMenuItem::new("Loading chats..."));
                    } else {
                        let mut seen_direct = std::collections::HashSet::new();
                        for room in direct_rooms {
                            let room_key = room.id.trim().to_string();
                            if !seen_direct.insert(room_key) {
                                continue;
                            }

                            let room_id = room.id.clone();
                            let room_name = room.display_name.clone();
                            let short_room_id = if room_id.len() > 12 {
                                format!("{}...", &room_id[..12])
                            } else {
                                room_id.clone()
                            };
                            let room_id_for_menu = room_id.clone();
                            let matrix_entity_for_menu = self.matrix.clone();
                            let leave_room_id = room_id_for_menu.clone();
                            let leave_room_matrix = matrix_entity_for_menu.clone();

                            secondary_menu =
                                secondary_menu.child(
                                    SidebarMenuItem::new(format!("{} ({})", room_name, short_room_id))
                                        .on_click(cx.listener(move |this: &mut AvernusApp, _event, _window, cx| {
                                            eprintln!("select room: {} ({})", room_id, room_name);
                                            this.active_item = Some(room_id.clone());
                                            let matrix_entity = this.matrix.clone();
                                            let room_id_for_history = room_id.clone();
                                            cx.spawn(async move |_this, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.load_room_history(cx, room_id_for_history.clone());
                                                    });
                                                });
                                            })
                                            .detach();
                                            cx.notify();
                                        }))
                                        .context_menu(move |menu, _, _| {
                                            let leave_room_id_for_click = leave_room_id.clone();
                                            let leave_room_matrix_for_click = leave_room_matrix.clone();
                                            menu.item(
                                                PopupMenuItem::new("Leave room").on_click(move |_, _, cx| {
                                                    let room_id = leave_room_id_for_click.clone();
                                                    let matrix_entity = leave_room_matrix_for_click.clone();
                                                    cx.spawn(async move |cx| {
                                                        let _ = cx.update(|cx| {
                                                            matrix_entity.update(cx, |backend, cx| {
                                                                backend.leave_room(cx, room_id.clone());
                                                            });
                                                        });
                                                    })
                                                    .detach();
                                                }),
                                            )
                                        }),
                                );
                        }
                    }
                } else if selection == "Rooms" {
                    secondary_menu = secondary_menu.child(SidebarMenuItem::new("+ New room").on_click(
                        cx.listener(|this: &mut AvernusApp, _event, _window, cx| {
                            this.active_dialog = Some(DialogType::NewRoom);
                            cx.notify();
                        }),
                    ));

                    let non_direct_rooms: Vec<_> = matrix_state.rooms.iter().filter(|r| !r.is_direct).cloned().collect();
                    if non_direct_rooms.is_empty() {
                        secondary_menu = secondary_menu.child(SidebarMenuItem::new("Loading rooms..."));
                    } else {
                        let mut seen_rooms = std::collections::HashSet::new();
                        for room in non_direct_rooms {
                            let room_key = room.id.trim().to_string();
                            if !seen_rooms.insert(room_key) {
                                continue;
                            }

                            let room_id = room.id.clone();
                            let room_name = room.display_name.clone();
                            let short_room_id = if room_id.len() > 12 {
                                format!("{}...", &room_id[..12])
                            } else {
                                room_id.clone()
                            };
                            let room_id_for_menu = room_id.clone();
                            let matrix_entity_for_menu = self.matrix.clone();
                            let leave_room_id = room_id_for_menu.clone();
                            let leave_room_matrix = matrix_entity_for_menu.clone();

                            secondary_menu =
                                secondary_menu.child(
                                    SidebarMenuItem::new(format!("{} ({})", room_name, short_room_id))
                                        .on_click(cx.listener(move |this: &mut AvernusApp, _event, _window, cx| {
                                            eprintln!("select room: {} ({})", room_id, room_name);
                                            this.active_item = Some(room_id.clone());
                                            let matrix_entity = this.matrix.clone();
                                            let room_id_for_history = room_id.clone();
                                            cx.spawn(async move |_this, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.load_room_history(cx, room_id_for_history.clone());
                                                    });
                                                });
                                            })
                                            .detach();
                                            cx.notify();
                                        }))
                                        .context_menu(move |menu, _, _| {
                                            let leave_room_id_for_click = leave_room_id.clone();
                                            let leave_room_matrix_for_click = leave_room_matrix.clone();
                                            menu.item(
                                                PopupMenuItem::new("Leave room").on_click(move |_, _, cx| {
                                                    let room_id = leave_room_id_for_click.clone();
                                                    let matrix_entity = leave_room_matrix_for_click.clone();
                                                    cx.spawn(async move |cx| {
                                                        let _ = cx.update(|cx| {
                                                            matrix_entity.update(cx, |backend, cx| {
                                                                backend.leave_room(cx, room_id.clone());
                                                            });
                                                        });
                                                    })
                                                    .detach();
                                                }),
                                            )
                                        }),
                                );
                        }
                    }
                } else if selection == "Settings" {
                    secondary_menu = secondary_menu.child(
                        SidebarMenuItem::new("Account Sign In")
                            .on_click(cx.listener(|this: &mut AvernusApp, _event, _window, cx| {
                                this.active_dialog = Some(DialogType::AccountSignIn);
                                cx.notify();
                            }))
                    );
                }

                let header_title = if selection == "Settings" {
                    "Settings".to_string()
                } else {
                    format!("{} ({})", selection, matrix_state.connection_status)
                };

                row = row.child(
                    Sidebar::new("secondary")
                        .w(rems(12.0))
                        .header(SidebarHeader::new().child(header_title))
                        .child(secondary_menu),
                );
            }
        }

        // --- Main Content View ---
        let main_content: gpui::AnyElement = if self.active_sidebar.as_deref() == Some("Settings") {
            v_flex()
                .flex_1()
                .h_full()
                .p_6()
                .gap_4()
                .child(div().text_xl().font_bold().child("Client Settings"))
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().font_bold().child("Matrix Account Session"))
                        .child(div().text_sm().child(format!(
                            "Logged in status: {}",
                            matrix_state.connection_status
                        ))),
                )
                .child(
                    v_flex()
                        .w(rems(26.0))
                        .gap_3()
                        .p_4()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(div().text_sm().font_bold().child("Encryption & verification"))
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(
                            "Verify the current Matrix session, or reset stale crypto state if Element keeps asking for a second-device approval.",
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("settings-verify-session")
                                        .label("Request device verification")
                                        .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                            let matrix_entity = this.matrix.clone();
                                            cx.spawn(async move |_this, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.verify_current_session(cx);
                                                    });
                                                });
                                            })
                                            .detach();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("settings-reset-crypto")
                                        .label("Reset stale encryption session")
                                        .with_variant(ButtonVariant::Danger)
                                        .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                            let matrix_entity = this.matrix.clone();
                                            cx.spawn(async move |_this, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.reset_stale_encryption_session(cx);
                                                    });
                                                });
                                            })
                                            .detach();
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .child(
                    v_flex()
                        .w(rems(26.0))
                        .gap_3()
                        .p_4()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(div().text_sm().font_bold().child("Sign in to Matrix"))
                        .child(v_flex().gap_2().child(
                            div().text_xs().font_bold().child("User ID")
                        ).child(
                            Input::new(&self.login_user).cleanable(true).large(),
                        ))
                        .child(v_flex().gap_2().child(
                            div().text_xs().font_bold().child("Password")
                        ).child(
                            Input::new(&self.login_password).cleanable(true).large(),
                        ))
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("settings-login")
                                        .label("Log in")
                                        .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                            let username = this
                                                .login_user
                                                .read(cx)
                                                .text()
                                                .to_string();
                                            let password = this
                                                .login_password
                                                .read(cx)
                                                .text()
                                                .to_string();

                                            let username = username.trim().to_string();
                                            let password = password.trim().to_string();
                                            if !username.is_empty() && !password.is_empty() {
                                                let matrix_entity = this.matrix.clone();
                                                cx.spawn(async move |_this, cx| {
                                                    let _ = cx.update(|cx| {
                                                        matrix_entity.update(cx, |backend, cx| {
                                                            backend.start_sync(cx, username, password);
                                                        });
                                                    });
                                                })
                                                .detach();
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("settings-browser-login")
                                        .label("Open browser sign in")
                                        .with_variant(ButtonVariant::Ghost)
                                        .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                            let homeserver = std::env::var("MATRIX_HOMESERVER")
                                                .or_else(|_| std::env::var("MATRIX_SERVER"))
                                                .unwrap_or_else(|_| "https://matrix.org".to_string());
                                            let matrix_entity = this.matrix.clone();
                                            cx.spawn(async move |_this, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.start_sso_login(cx, homeserver);
                                                    });
                                                });
                                            })
                                            .detach();
                                            this.active_dialog = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("settings-logout")
                                        .label("Log out")
                                        .with_variant(ButtonVariant::Danger)
                                        .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                            let matrix_entity = this.matrix.clone();
                                            cx.spawn(async move |_this, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.logout(cx);
                                                    });
                                                });
                                            })
                                            .detach();
                                            this.active_sidebar = Some("Settings".to_string());
                                            this.active_item = None;
                                            this.active_dialog = None;
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .into_any_element()
        } else if self.active_sidebar.as_deref() == Some("Space: Discover") {
            self.render_discover_panel(window, cx).into_any_element()
        } else {
            match &self.active_item {
                Some(item) => {
                    let room_messages = matrix_state
                        .messages
                        .get(item)
                        .cloned()
                        .unwrap_or_default();

                    let room_name = matrix_state
                        .rooms
                        .iter()
                        .find(|room| room.id == *item)
                        .map(|room| room.display_name.clone())
                        .unwrap_or_else(|| item.clone());

                    let short_room_id = if item.len() > 12 {
                        format!("{}...", &item[..12])
                    } else {
                        item.clone()
                    };

                    let current_room_message_count = room_messages.len();
                    let previous_count = self.last_message_count.get(item).copied().unwrap_or(0);
                    if current_room_message_count > previous_count && !room_messages.is_empty() {
                        self.message_scroll_handle.scroll_to_bottom();
                    }
                    self.last_message_count.insert(item.clone(), current_room_message_count);

                    v_flex()
                        .flex_1()
                        .h_full()
                        .min_h_0()
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .p_3()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(div().font_bold().child(format!("{} ({})", room_name, short_room_id)))
                                        .child(
                                            div().text_xs().child("🔒 Encrypted"),
                                        ),
                                )
                                .child(
                                    Button::new("toggle-room-details")
                                        .icon(IconName::User)
                                        .with_variant(if self.show_room_details {
                                            ButtonVariant::Secondary
                                        } else {
                                            ButtonVariant::Ghost
                                        })
                                        .on_click(cx.listener(|this: &mut AvernusApp, _, _, cx| {
                                            this.show_room_details = !this.show_room_details;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("room-messages-scroll")
                                .flex_1()
                                .h_full()
                                .min_h_0()
                                .track_scroll(&self.message_scroll_handle)
                                .overflow_y_scroll()
                                .p_4()
                                .child(
                                    if room_messages.is_empty() {
                                        div().text_sm().child("No messages in this room yet.")
                                    } else {
                                        v_flex()
                                            .gap_2()
                                            .children(room_messages.iter().map(|message| {
                                                let sender_label = if message.sender == "you" {
                                                    None
                                                } else {
                                                    Some(div().text_xs().font_bold().child(message.sender.clone()))
                                                };

                                                div()
                                                    .p_2()
                                                    .rounded_md()
                                                    .bg(cx.theme().background)
                                                    .child(
                                                        v_flex()
                                                            .gap_1()
                                                            .children(sender_label)
                                                            .child(div().text_sm().child(message.body.clone())),
                                                    )
                                            }))
                                    },
                                ),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .p_2()
                                .gap_2()
                                .child(
                                    Button::new("attach-file-btn")
                                        .with_variant(ButtonVariant::Secondary)
                                        .child(div().child("Attach"))
                                        .on_click(cx.listener(|this: &mut AvernusApp, _, window, cx| {
                                            let Some(room_id) = this.active_item.clone() else {
                                                return;
                                            };

                                            let Some(path) = rfd::FileDialog::new()
                                                .set_title("Select a file to send")
                                                .pick_file()
                                            else {
                                                return;
                                            };

                                            let caption = {
                                                let input = this.message_input.read(cx);
                                                let text = input.text().to_string();
                                                let text = text.trim().to_string();
                                                if text.is_empty() { None } else { Some(text) }
                                            };

                                            this.message_input.update(cx, |input_state, cx| {
                                                input_state.set_value("", window, cx);
                                            });

                                            let matrix_entity = this.matrix.clone();
                                            cx.spawn(async move |_, cx| {
                                                let _ = cx.update(|cx| {
                                                    matrix_entity.update(cx, |backend, cx| {
                                                        backend.send_attachment(cx, room_id.clone(), path.clone(), caption.clone());
                                                    });
                                                });
                                            })
                                            .detach();
                                        })),
                                )
                                .child(Input::new(&self.message_input).cleanable(true).large())
                        )
                        .into_any_element()
                }
                None => div().flex_1().h_full().into_any_element(),
            }
        };

        row = row.child(main_content);

        if self.show_room_details && self.active_item.is_some() {
            row = row.child(self.render_room_details_panel(cx));
        }

        let mut app_layout = row.children(dialog_layer);

        if self.matrix.read(cx).pending_verification.is_some() {
            let modal = self.render_verification_challenge_modal(cx);
            app_layout = app_layout.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(gpui::black().opacity(0.40))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(modal),
            );
        } else if self.active_dialog.is_some() {
            let modal = self.render_modal_dialog(window, cx);
            app_layout = app_layout.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(gpui::black().opacity(0.40))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(modal),
            );
        }

        app_layout
    }
}