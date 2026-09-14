use gpui::*;

mod app_layout;
mod matrix;

use app_layout::{AvernusApp, RoomDetailsTab};
use gpui_component::input::InputState;
use matrix::MatrixBackend;

fn main() {
    // 1. Initialize Tokio Runtime for matrix_sdk async database & network tasks
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    // 2. Enter the runtime context so matrix-sdk finds a valid reactor
    let _guard = tokio_runtime.enter();

    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);

        // 3. Instantiate MatrixBackend GPUI Model
        let matrix = MatrixBackend::new(cx);

        let username = std::env::var("MATRIX_USERNAME")
            .or_else(|_| std::env::var("MATRIX_USER"))
            .unwrap_or_else(|_| "".to_string());
        let password = std::env::var("MATRIX_PASSWORD").unwrap_or_else(|_| "".to_string());
        let has_credentials = !username.is_empty() && !password.is_empty();
        let has_saved_session = std::path::Path::new("./avernus_matrix_store")
            .join("session.json")
            .exists();

        // 4. Start background login & room sync only when real credentials are configured.
        if has_credentials {
            matrix.update(cx, |backend, cx| {
                backend.start_sync(cx, username.clone(), password.clone());
            });
        } else {
            matrix.update(cx, |backend, cx| {
                backend.restore_saved_session(cx);
            });

            if !has_credentials && !has_saved_session {
                eprintln!(
                    "Matrix credentials not configured and no saved session exists yet. Set MATRIX_USERNAME and MATRIX_PASSWORD or sign in once in the app."
                );
            }
        }

        // 5. Open window and bind view models
        cx.spawn(async move |cx| {
            let _ = cx.update(|cx| {
                cx.open_window(WindowOptions::default(), |window, cx| {
                    // Create main chat input
                    let chat_input = cx.new(|model_cx| {
                        InputState::new(window, model_cx)
                            .placeholder("Message...")
                            .submit_on_enter(true)
                    });

                    // Create secondary input for modal dialog search/input
                    let modal_input = cx.new(|model_cx| {
                        InputState::new(window, model_cx).placeholder("Search or enter name...")
                    });

                    let login_user_input = cx.new(|model_cx| {
                        InputState::new(window, model_cx).placeholder("@user:matrix.org")
                    });

                    let login_password_input = cx.new(|model_cx| {
                        InputState::new(window, model_cx)
                            .placeholder("Password")
                            .masked(true)
                    });

                    // Initialize view model with Matrix backend model
                    let view = cx.new(|cx| {
                        let matrix_entity = matrix.clone();
                        let matrix_subscription = cx.subscribe(&matrix_entity, |_, _, _, cx| {
                            cx.notify();
                        });

                        let message_input_subscription = cx.subscribe_in(
                            &chat_input,
                            window,
                            move |this: &mut AvernusApp, _, event: &gpui_component::input::InputEvent, window, cx| {
                                if let gpui_component::input::InputEvent::PressEnter { .. } = event {
                                    let room_id = this.active_item.clone();
                                    let room_id = match room_id {
                                        Some(room_id) => room_id,
                                        None => return,
                                    };

                                    let message = {
                                        let input = this.message_input.read(cx);
                                        input.text().to_string()
                                    };
                                    let message = message.trim().to_string();
                                    if message.is_empty() {
                                        return;
                                    }

                                    this.message_input.update(cx, |input_state, cx| {
                                        input_state.set_value("", window, cx);
                                    });

                                    let matrix_entity = this.matrix.clone();
                                    cx.spawn(async move |_, cx| {
                                        let _ = cx.update(|cx| {
                                            matrix_entity.update(cx, |backend, cx| {
                                                backend.send_message(cx, room_id.clone(), message.clone());
                                            });
                                        });
                                    })
                                    .detach();
                                }
                            },
                        );

                        let discover_input_subscription = cx.subscribe_in(
                            &modal_input,
                            window,
                            move |this: &mut AvernusApp, _, event: &gpui_component::input::InputEvent, _window, cx| {
                                if let gpui_component::input::InputEvent::PressEnter { .. } = event {
                                    if !matches!(this.active_sidebar.as_deref(), Some("Space: Discover")) {
                                        return;
                                    }

                                    let query = this.dialog_input.read(cx).text().to_string();
                                    let query = query.trim().to_string();
                                    if query.is_empty() {
                                        return;
                                    }

                                    let matrix_entity = this.matrix.clone();
                                    cx.spawn(async move |_this, cx| {
                                        let _ = cx.update(|cx| {
                                            matrix_entity.update(cx, |backend, cx| {
                                                if query.starts_with('#') || query.starts_with('!') || query.starts_with("http://") || query.starts_with("https://") {
                                                    backend.join_room_by_id_or_alias(cx, query);
                                                } else {
                                                    backend.search_public_rooms(cx, query);
                                                }
                                            });
                                        });
                                    })
                                    .detach();
                                    cx.notify();
                                }
                            },
                        );

                        AvernusApp {
                            matrix: matrix.clone(),
                            active_sidebar: None,
                            active_item: None,
                            message_input: chat_input.clone(),
                            dialog_input: modal_input,
                            login_user: login_user_input,
                            login_password: login_password_input,
                            active_dialog: None,
                            show_room_details: false,
                            room_details_tab: RoomDetailsTab::Members,
                            joined_spaces: vec![],
                            message_scroll_handle: ScrollHandle::new(),
                            last_message_count: Default::default(),
                            matrix_subscription,
                            message_input_subscription,
                            discover_input_subscription,
                        }
                    });

                    cx.new(|model_cx| gpui_component::Root::new(view, window, model_cx))
                })
            });
        })
        .detach();
    });
}