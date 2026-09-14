use futures_util::StreamExt;
use gpui::{*, EventEmitter};
use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoomRequest;
use matrix_sdk::ruma::events::room::message::{
    MessageType, RoomMessageEventContent, SyncRoomMessageEvent, TextMessageEventContent,
};
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyToDeviceEvent, SyncMessageLikeEvent,
};
use matrix_sdk::ruma::{assign, RoomId, RoomOrAliasId, UserId};
use matrix_sdk::{config::SyncSettings, Client, Room};
use smol::channel::{unbounded, Sender};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub struct RoomInfo {
    pub id: String,
    pub display_name: String,
    pub is_direct: bool,
}

#[derive(Clone, Debug)]
pub struct MatrixMessage {
    pub room_id: String,
    pub sender: String,
    pub body: String,
    pub timestamp: u64,
    pub event_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PublicRoomSearchResult {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub alias: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationEmoji {
    pub symbol: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingVerificationChallenge {
    pub user_id: String,
    pub flow_id: String,
    pub emojis: Vec<VerificationEmoji>,
}

#[derive(Clone, Debug)]
pub enum MatrixBridgeEvent {
    Connected { generation: u64 },
    SyncRooms { generation: u64, rooms: Vec<RoomInfo> },
    PublicRoomsDiscovered {
        generation: u64,
        rooms: Vec<PublicRoomSearchResult>,
    },
    RoomHistory {
        generation: u64,
        room_id: String,
        messages: Vec<MatrixMessage>,
    },
    NewMessage { generation: u64, message: MatrixMessage },
    VerificationChallenge {
        generation: u64,
        user_id: String,
        flow_id: String,
        emojis: Vec<VerificationEmoji>,
    },
    ConnectionFailed { generation: u64, error: String },
}

impl EventEmitter<MatrixBridgeEvent> for MatrixBackend {}

pub struct MatrixBackend {
    pub rooms: Vec<RoomInfo>,
    pub messages: std::collections::HashMap<String, Vec<MatrixMessage>>,
    pub public_rooms: Vec<PublicRoomSearchResult>,
    pub pending_verification: Option<PendingVerificationChallenge>,
    pub connection_status: String,
    client: Arc<RwLock<Option<Client>>>,
    event_tx: Sender<MatrixBridgeEvent>,
    sync_generation: Arc<AtomicU64>,
}

impl MatrixBackend {
    fn is_same_message(lhs: &MatrixMessage, rhs: &MatrixMessage) -> bool {
        if lhs.room_id != rhs.room_id {
            return false;
        }

        if let (Some(lhs_event_id), Some(rhs_event_id)) = (&lhs.event_id, &rhs.event_id) {
            return lhs_event_id == rhs_event_id;
        }

        if lhs.sender != rhs.sender || lhs.body != rhs.body {
            return false;
        }

        let time_delta_ms = lhs.timestamp.abs_diff(rhs.timestamp);
        time_delta_ms <= 5_000
    }

    fn merge_room_history(existing: &mut Vec<MatrixMessage>, incoming: Vec<MatrixMessage>) {
        for message in incoming {
            let already_present = existing.iter().any(|current| Self::is_same_message(current, &message));

            if !already_present {
                existing.push(message);
            }
        }

        existing.sort_by_key(|message| message.timestamp);
    }

    fn dedupe_rooms(rooms: Vec<RoomInfo>) -> Vec<RoomInfo> {
        let mut deduped = Vec::new();
        let mut seen_room_ids = std::collections::HashSet::new();

        for room in rooms {
            let room_key = room.id.trim().to_string();
            if !seen_room_ids.insert(room_key.clone()) {
                continue;
            }

            deduped.push(room);
        }

        deduped
    }

    fn session_file_path() -> PathBuf {
        PathBuf::from("./avernus_matrix_store").join("session.json")
    }

    fn clear_session_state() {
        let store_path = PathBuf::from("./avernus_matrix_store");
        if store_path.exists() {
            let _ = std::fs::remove_dir_all(&store_path);
        }
        let _ = std::fs::remove_file(Self::session_file_path());
        let _ = std::fs::create_dir_all(&store_path);
    }

    fn reset_store_for_account(store_path: &PathBuf) {
        if store_path.exists() {
            let _ = std::fs::remove_dir_all(store_path);
        }
        let _ = std::fs::create_dir_all(store_path);
        let _ = std::fs::remove_file(Self::session_file_path());
    }

    fn persist_session(client: &Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let session = client
            .matrix_auth()
            .session()
            .ok_or("No active Matrix session to persist")?;
        let path = Self::session_file_path();
        std::fs::create_dir_all("./avernus_matrix_store")?;
        std::fs::write(path, serde_json::to_string_pretty(&session)?)?;
        Ok(())
    }

    fn load_session() -> Option<matrix_sdk::authentication::matrix::MatrixSession> {
        let path = Self::session_file_path();
        let file = std::fs::read_to_string(path).ok()?;
        let session: matrix_sdk::authentication::matrix::MatrixSession =
            serde_json::from_str(&file).ok()?;
        Some(session)
    }

    fn attachment_content_type(path: &std::path::Path) -> mime::Mime {
        mime_guess::from_path(path).first_or_octet_stream()
    }

    fn attachment_display_name(path: &std::path::Path) -> String {
        path.file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("attachment")
            .to_string()
    }

    fn emit_sas_challenge(
        tx: &Sender<MatrixBridgeEvent>,
        generation: u64,
        user_id: &str,
        flow_id: &str,
        short_auth: Option<matrix_sdk::encryption::verification::EmojiShortAuthString>,
    ) {
        let Some(short_auth) = short_auth else {
            return;
        };

        let challenge = PendingVerificationChallenge {
            user_id: user_id.to_string(),
            flow_id: flow_id.to_string(),
            emojis: short_auth
                .emojis
                .iter()
                .map(|emoji| VerificationEmoji {
                    symbol: emoji.symbol.to_string(),
                    description: emoji.description.to_string(),
                })
                .collect(),
        };

        let _ = tx.try_send(MatrixBridgeEvent::VerificationChallenge {
            generation,
            user_id: challenge.user_id.clone(),
            flow_id: challenge.flow_id.clone(),
            emojis: challenge.emojis.clone(),
        });
    }

    fn wait_for_sas_challenge(
        tx: &Sender<MatrixBridgeEvent>,
        generation: u64,
        user_id: &str,
        flow_id: &str,
        sas_verification: matrix_sdk::encryption::verification::SasVerification,
    ) {
        let tx = tx.clone();
        let user_id = user_id.to_string();
        let flow_id = flow_id.to_string();

        tokio::spawn(async move {
            match sas_verification.state() {
                matrix_sdk::encryption::verification::SasState::KeysExchanged { emojis, .. } => {
                    Self::emit_sas_challenge(&tx, generation, &user_id, &flow_id, emojis);
                    return;
                }
                _ => {}
            }

            let mut states = sas_verification.changes();
            while let Some(state) = states.next().await {
                match state {
                    matrix_sdk::encryption::verification::SasState::KeysExchanged { emojis, .. } => {
                        Self::emit_sas_challenge(&tx, generation, &user_id, &flow_id, emojis);
                        break;
                    }
                    matrix_sdk::encryption::verification::SasState::Cancelled(_) => break,
                    matrix_sdk::encryption::verification::SasState::Done { .. } => break,
                    _ => {}
                }
            }
        });
    }

    fn install_verification_handlers(client: &Client, tx: &Sender<MatrixBridgeEvent>, generation: u64) {
        let client_for_handler = client.clone();
        let tx_for_handler = tx.clone();

        client.add_event_handler(move |event: AnyToDeviceEvent| {
            let client = client_for_handler.clone();
            let tx = tx_for_handler.clone();
            let generation = generation;

            async move {
                let (user_id, flow_id) = match event {
                    AnyToDeviceEvent::KeyVerificationRequest(event) => {
                        (event.sender.clone(), event.content.transaction_id.to_string())
                    }
                    _ => return,
                };

                let Some(verification_request) = client
                    .encryption()
                    .get_verification_request(&user_id, flow_id.as_str())
                    .await
                else {
                    return;
                };

                if verification_request.is_done()
                    || verification_request.is_cancelled()
                    || verification_request.we_started()
                {
                    return;
                }

                let Ok(()) = verification_request.accept().await else {
                    return;
                };

                let Ok(Some(sas_verification)) = verification_request.start_sas().await else {
                    return;
                };

                Self::wait_for_sas_challenge(
                    &tx,
                    generation,
                    user_id.as_str(),
                    flow_id.as_str(),
                    sas_verification,
                );
            }
        });
    }

    async fn collect_joined_rooms(client: &Client) -> Vec<RoomInfo> {
        let mut rooms = Vec::new();

        for room in client.joined_rooms() {
            let display_name = room
                .display_name()
                .await
                .map(|name| name.to_string())
                .unwrap_or_else(|_| room.room_id().to_string());

            let is_direct = room.is_direct().await.unwrap_or(false);
            let room_id = room.room_id().to_string();

            rooms.push(RoomInfo {
                id: room_id,
                display_name,
                is_direct,
            });
        }

        rooms
    }

    async fn refresh_joined_rooms(client: &Client, tx: &Sender<MatrixBridgeEvent>, generation: u64) {
        let rooms = Self::collect_joined_rooms(client).await;
        let _ = tx.send(MatrixBridgeEvent::SyncRooms { generation, rooms }).await;
    }

    pub fn new(cx: &mut App) -> Entity<Self> {
        let (tx, rx) = unbounded::<MatrixBridgeEvent>();

        cx.new(|cx| {
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                while let Ok(event) = rx.recv().await {
                    let event_for_emit = event.clone();
                    let update_succeeded = this
                        .update(cx, |backend, cx| {
                            let current_generation = backend.sync_generation.load(Ordering::SeqCst);

                            match event {
                                MatrixBridgeEvent::Connected { generation } => {
                                    if generation != current_generation {
                                        return;
                                    }
                                    backend.connection_status = "Connected".to_string();
                                }
                                MatrixBridgeEvent::SyncRooms { generation, rooms } => {
                                    if generation != current_generation {
                                        return;
                                    }
                                    let unique_rooms = Self::dedupe_rooms(rooms);

                                    backend.rooms = unique_rooms.clone();
                                    backend.messages.retain(|room_id, _| unique_rooms.iter().any(|room| room.id == *room_id));
                                    for room in unique_rooms {
                                        eprintln!("sync room: {} ({})", room.id, room.display_name);
                                    }
                                }
                                MatrixBridgeEvent::PublicRoomsDiscovered { generation, rooms } => {
                                    if generation != current_generation {
                                        return;
                                    }
                                    backend.public_rooms = rooms;
                                }
                                MatrixBridgeEvent::RoomHistory {
                                    generation,
                                    room_id,
                                    messages,
                                } => {
                                    if generation != current_generation {
                                        return;
                                    }
                                    let entry = backend.messages.entry(room_id).or_default();
                                    Self::merge_room_history(entry, messages);
                                }
                                MatrixBridgeEvent::NewMessage { generation, message } => {
                                    if generation != current_generation {
                                        return;
                                    }
                                    let entry = backend.messages.entry(message.room_id.clone()).or_default();
                                    let already_present = entry.iter().any(|current| Self::is_same_message(current, &message));

                                    if !already_present {
                                        entry.push(message);
                                    }
                                }
                                MatrixBridgeEvent::VerificationChallenge {
                                    generation,
                                    user_id,
                                    flow_id,
                                    emojis,
                                } => {
                                    if generation != current_generation {
                                        return;
                                    }
                                    backend.pending_verification = Some(PendingVerificationChallenge {
                                        user_id,
                                        flow_id,
                                        emojis,
                                    });
                                }
                                MatrixBridgeEvent::ConnectionFailed { generation, error } => {
                                    if generation != current_generation {
                                        return;
                                    }
                                    backend.pending_verification = None;
                                    backend.connection_status = format!("Error: {}", error);
                                }
                            }
                            cx.emit(event_for_emit.clone());
                        })
                        .is_ok();

                    if !update_succeeded {
                        break;
                    }
                }
            })
            .detach();

            Self {
                rooms: Vec::new(),
                messages: std::collections::HashMap::new(),
                public_rooms: Vec::new(),
                pending_verification: None,
                connection_status: "Offline".to_string(),
                client: Arc::new(RwLock::new(None)),
                event_tx: tx,
                sync_generation: Arc::new(AtomicU64::new(0)),
            }
        })
    }

    pub fn restore_saved_session(&self, cx: &mut Context<Self>) {
        let tx = self.event_tx.clone();
        let client_arc = Arc::clone(&self.client);
        let rt_handle = tokio::runtime::Handle::current();
        let generation = self.sync_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation_arc = Arc::clone(&self.sync_generation);

        cx.background_executor().spawn(async move {
            rt_handle.spawn(async move {
                let Some(session) = Self::load_session() else {
                    return;
                };

                let server_name = session.meta.user_id.server_name();
                let store_path = PathBuf::from("./avernus_matrix_store");

                let client = match Client::builder()
                    .server_name(server_name)
                    .sqlite_store(store_path.clone(), None)
                    .build()
                    .await
                {
                    Ok(client) => client,
                    Err(error) => {
                        let _ = tx
                            .send(MatrixBridgeEvent::ConnectionFailed {
                                generation,
                                error: error.to_string(),
                            })
                            .await;
                        return;
                    }
                };

                client
                    .set_session_callbacks(
                        Box::new({
                            let session_path = Arc::new(Self::session_file_path());
                            move |_| {
                                let file = std::fs::read_to_string(session_path.as_path())?;
                                let session: matrix_sdk::authentication::matrix::MatrixSession =
                                    serde_json::from_str(&file)?;
                                Ok(session.tokens)
                            }
                        }),
                        Box::new({
                            let session_path = Arc::new(Self::session_file_path());
                            move |client| {
                                std::fs::create_dir_all("./avernus_matrix_store")?;
                                let session = client
                                    .matrix_auth()
                                    .session()
                                    .ok_or("No active Matrix session to persist")?;
                                std::fs::write(session_path.as_path(), serde_json::to_string_pretty(&session)?)?;
                                Ok(())
                            }
                        }),
                    )
                    .expect("session callback can only be set once");

                if let Err(error) = client.restore_session(session).await {
                    Self::clear_session_state();
                    let _ = tx
                        .send(MatrixBridgeEvent::ConnectionFailed {
                            generation,
                            error: format!("Saved Matrix session is stale or invalid: {error}"),
                        })
                        .await;
                    return;
                }

                if !Self::ensure_cross_signing_bootstrapped(&client).await {
                    let _ = tx
                        .send(MatrixBridgeEvent::ConnectionFailed {
                            generation,
                            error: "Matrix device trust state was stale; please log in again to reset the encryption session."
                                .to_string(),
                        })
                        .await;
                    return;
                }

                Self::install_verification_handlers(&client, &tx, generation);

                if let Err(error) = Self::persist_session(&client) {
                    eprintln!("Failed to persist restored Matrix session: {error}");
                }

                let client_for_sync = client.clone();
                let tx_for_sync = tx.clone();
                {
                    let mut lock = client_arc.write().await;
                    *lock = Some(client.clone());
                }

                client.add_event_handler({
                    let tx = tx.clone();
                    let generation = generation;
                    move |event: SyncRoomMessageEvent, room: Room| async move {
                        let Some(original) = (match event {
                            SyncRoomMessageEvent::Original(original) => Some(original),
                            SyncRoomMessageEvent::Redacted(_) => None,
                        }) else {
                            return;
                        };

                        if let MessageType::Text(text_content) = original.content.msgtype {
                            let msg = MatrixMessage {
                                room_id: room.room_id().to_string(),
                                sender: original.sender.to_string(),
                                body: text_content.body.clone(),
                                timestamp: original.origin_server_ts.as_secs().into(),
                                event_id: Some(original.event_id.to_string()),
                            };
                            let _ = tx.send(MatrixBridgeEvent::NewMessage { generation, message: msg }).await;
                        }
                    }
                });

                if generation_arc.load(Ordering::SeqCst) != generation {
                    return;
                }

                let initial_rooms = Self::collect_joined_rooms(&client_for_sync).await;
                if generation_arc.load(Ordering::SeqCst) != generation {
                    return;
                }
                let _ = tx.send(MatrixBridgeEvent::SyncRooms { generation, rooms: initial_rooms }).await;
                let _ = tx.send(MatrixBridgeEvent::Connected { generation }).await;

                let mut sync_settings = SyncSettings::default();
                loop {
                    if generation_arc.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    match client_for_sync.sync_once(sync_settings.clone()).await {
                        Ok(response) => {
                            sync_settings = sync_settings.token(response.next_batch);
                            let mut rooms = Vec::new();

                            for room in client_for_sync.joined_rooms() {
                                let display_name = room
                                    .display_name()
                                    .await
                                    .map(|name| name.to_string())
                                    .unwrap_or_else(|_| room.room_id().to_string());

                                let is_direct = room.is_direct().await.unwrap_or(false);
                                let room_id = room.room_id().to_string();

                                eprintln!("joined_room: {} | display_name: {} | is_direct: {} | ACTIVE", room_id, display_name, is_direct);
                                rooms.push(RoomInfo {
                                    id: room_id,
                                    display_name,
                                    is_direct,
                                });
                            }

                            if generation_arc.load(Ordering::SeqCst) != generation {
                                return;
                            }
                            let _ = tx_for_sync.send(MatrixBridgeEvent::SyncRooms { generation, rooms }).await;
                        }
                        Err(error) => {
                            eprintln!("Matrix Sync Error: {error}");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            });
        }).detach();
    }

    pub fn start_sync(
        &self,
        cx: &mut Context<Self>,
        username_input: String,
        password: String,
    ) {
        let tx = self.event_tx.clone();
        let client_arc = Arc::clone(&self.client);
        let rt_handle = tokio::runtime::Handle::current();
        let generation = self.sync_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation_arc = Arc::clone(&self.sync_generation);

        cx.background_executor()
            .spawn(async move {
                rt_handle.spawn(async move {
                    let (user_id, localpart) = if username_input.starts_with('@') {
                        match UserId::parse(&username_input) {
                            Ok(id) => {
                                let local = id.localpart().to_string();
                                (id, local)
                            }
                            Err(error) => {
                                let _ = tx
                                    .send(MatrixBridgeEvent::ConnectionFailed {
                                        generation,
                                        error: format!("Invalid user ID format: {error}"),
                                    })
                                    .await;
                                return;
                            }
                        }
                    } else {
                        let formatted = format!("@{}:matrix.org", username_input);
                        match UserId::parse(&formatted) {
                            Ok(id) => (id, username_input.clone()),
                            Err(error) => {
                                let _ = tx
                                    .send(MatrixBridgeEvent::ConnectionFailed {
                                        generation,
                                        error: format!("Invalid username format: {error}"),
                                    })
                                    .await;
                                return;
                            }
                        }
                    };

                    let store_path = PathBuf::from("./avernus_matrix_store");

                    let client = match Client::builder()
                        .server_name(user_id.server_name())
                        .sqlite_store(store_path.clone(), None)
                        .build()
                        .await
                    {
                        Ok(client) => client,
                        Err(error) => {
                            let message = error.to_string();
                            if message.contains("the account in the store doesn't match") {
                                Self::reset_store_for_account(&store_path);
                                match Client::builder()
                                    .server_name(user_id.server_name())
                                    .sqlite_store(store_path.clone(), None)
                                    .build()
                                    .await
                                {
                                    Ok(client) => client,
                                    Err(error) => {
                                        let _ = tx
                                            .send(MatrixBridgeEvent::ConnectionFailed {
                                                generation,
                                                error: error.to_string(),
                                            })
                                            .await;
                                        return;
                                    }
                                }
                            } else {
                                let _ = tx
                                    .send(MatrixBridgeEvent::ConnectionFailed {
                                        generation,
                                        error: message,
                                    })
                                    .await;
                                return;
                            }
                        }
                    };

                    client
                        .set_session_callbacks(
                            Box::new({
                                let session_path = Arc::new(Self::session_file_path());
                                move |_| {
                                    let file = std::fs::read_to_string(session_path.as_path())?;
                                    let session: matrix_sdk::authentication::matrix::MatrixSession =
                                        serde_json::from_str(&file)?;
                                    Ok(session.tokens)
                                }
                            }),
                            Box::new({
                                let session_path = Arc::new(Self::session_file_path());
                                move |client| {
                                    std::fs::create_dir_all("./avernus_matrix_store")?;
                                    let session = client
                                        .matrix_auth()
                                        .session()
                                        .ok_or("No active Matrix session to persist")?;
                                    std::fs::write(session_path.as_path(), serde_json::to_string_pretty(&session)?)?;
                                    Ok(())
                                }
                            }),
                        )
                        .expect("session callback can only be set once");

                    if !client.matrix_auth().logged_in() {
                        if let Err(error) = client
                            .matrix_auth()
                            .login_username(&localpart, &password)
                            .initial_device_display_name("Avernus GPUI")
                            .send()
                            .await
                        {
                            let _ = tx
                                .send(MatrixBridgeEvent::ConnectionFailed {
                                    generation,
                                    error: error.to_string(),
                                })
                                .await;
                            return;
                        }
                    }

                    if !Self::ensure_cross_signing_bootstrapped(&client).await {
                        let _ = tx
                            .send(MatrixBridgeEvent::ConnectionFailed {
                                generation,
                                error: "Matrix device trust state was stale; please log in again to reset the encryption session."
                                    .to_string(),
                            })
                            .await;
                        return;
                    }

                    Self::install_verification_handlers(&client, &tx, generation);

                    if let Err(error) = Self::persist_session(&client) {
                        eprintln!("Failed to persist Matrix session after login: {error}");
                    }

                    let client_for_sync = client.clone();
                    let tx_for_sync = tx.clone();
                    let _client_arc_for_sync = Arc::clone(&client_arc);

                    client.add_event_handler({
                        let tx = tx.clone();
                        let generation = generation;
                        move |event: SyncRoomMessageEvent, room: Room| async move {
                            let Some(original) = (match event {
                                SyncRoomMessageEvent::Original(original) => Some(original),
                                SyncRoomMessageEvent::Redacted(_) => None,
                            }) else {
                                return;
                            };

                            if let MessageType::Text(text_content) = original.content.msgtype {
                                let msg = MatrixMessage {
                                    room_id: room.room_id().to_string(),
                                    sender: original.sender.to_string(),
                                    body: text_content.body.clone(),
                                    timestamp: original.origin_server_ts.get().into(),
                                    event_id: Some(original.event_id.to_string()),
                                };
                                let _ = tx.send(MatrixBridgeEvent::NewMessage { generation, message: msg }).await;
                            }
                        }
                    });

                    {
                        let mut lock = client_arc.write().await;
                        *lock = Some(client.clone());
                    }

                    if generation_arc.load(Ordering::SeqCst) != generation {
                        return;
                    }

                    let initial_rooms = Self::collect_joined_rooms(&client_for_sync).await;
                    if generation_arc.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    let _ = tx_for_sync
                        .send(MatrixBridgeEvent::SyncRooms { generation, rooms: initial_rooms })
                        .await;
                    let _ = tx.send(MatrixBridgeEvent::Connected { generation }).await;

                    let mut sync_settings = SyncSettings::default();

                    loop {
                        if generation_arc.load(Ordering::SeqCst) != generation {
                            return;
                        }
                        match client_for_sync.sync_once(sync_settings.clone()).await {
                            Ok(response) => {
                                sync_settings = sync_settings.token(response.next_batch);

                                let mut rooms = Vec::new();

                                for room in client_for_sync.joined_rooms() {
                                    let display_name = room
                                        .display_name()
                                        .await
                                        .map(|name| name.to_string())
                                        .unwrap_or_else(|_| room.room_id().to_string());

                                    let is_direct = room.is_direct().await.unwrap_or(false);

                                    let room_id = room.room_id().to_string();
                                    rooms.push(RoomInfo {
                                        id: room_id,
                                        display_name,
                                        is_direct,
                                    });
                                }

                                if generation_arc.load(Ordering::SeqCst) != generation {
                                    return;
                                }
                                let _ = tx_for_sync
                                    .send(MatrixBridgeEvent::SyncRooms { generation, rooms })
                                    .await;
                            }
                            Err(error) => {
                                eprintln!("Matrix Sync Error: {error}");
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                        }
                    }
                });
            })
            .detach();
    }

    pub fn start_sso_login(&self, cx: &mut Context<Self>, homeserver: String) {
        let tx = self.event_tx.clone();
        let client_arc = Arc::clone(&self.client);
        let rt_handle = tokio::runtime::Handle::current();
        let generation = self.sync_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation_arc = Arc::clone(&self.sync_generation);

        cx.background_executor()
            .spawn(async move {
                rt_handle.spawn(async move {
                    let server_name = homeserver
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/');

                    if server_name.is_empty() {
                        let _ = tx
                            .send(MatrixBridgeEvent::ConnectionFailed {
                                generation,
                                error: "Invalid homeserver URL. Use https://matrix.org or another Matrix server."
                                    .to_string(),
                            })
                            .await;
                        return;
                    }

                    let server_name = match <&matrix_sdk::ruma::ServerName>::try_from(server_name) {
                        Ok(server_name) => server_name,
                        Err(error) => {
                            let _ = tx
                                .send(MatrixBridgeEvent::ConnectionFailed {
                                    generation,
                                    error: format!("Invalid homeserver URL: {error}"),
                                })
                                .await;
                            return;
                        }
                    };

                    let store_path = PathBuf::from("./avernus_matrix_store");

                    let client = match Client::builder()
                        .server_name(server_name)
                        .sqlite_store(store_path.clone(), None)
                        .build()
                        .await
                    {
                        Ok(client) => client,
                        Err(error) => {
                            let message = error.to_string();
                            if message.contains("the account in the store doesn't match") {
                                Self::reset_store_for_account(&store_path);
                                match Client::builder()
                                    .server_name(server_name)
                                    .sqlite_store(store_path.clone(), None)
                                    .build()
                                    .await
                                {
                                    Ok(client) => client,
                                    Err(error) => {
                                        let _ = tx
                                            .send(MatrixBridgeEvent::ConnectionFailed {
                                                generation,
                                                error: error.to_string(),
                                            })
                                            .await;
                                        return;
                                    }
                                }
                            } else {
                                let _ = tx
                                    .send(MatrixBridgeEvent::ConnectionFailed {
                                        generation,
                                        error: message,
                                    })
                                    .await;
                                return;
                            }
                        }
                    };

                    client
                        .set_session_callbacks(
                            Box::new({
                                let session_path = Arc::new(Self::session_file_path());
                                move |_| {
                                    let file = std::fs::read_to_string(session_path.as_path())?;
                                    let session: matrix_sdk::authentication::matrix::MatrixSession =
                                        serde_json::from_str(&file)?;
                                    Ok(session.tokens)
                                }
                            }),
                            Box::new({
                                let session_path = Arc::new(Self::session_file_path());
                                move |client| {
                                    std::fs::create_dir_all("./avernus_matrix_store")?;
                                    let session = client
                                        .matrix_auth()
                                        .session()
                                        .ok_or("No active Matrix session to persist")?;
                                    std::fs::write(session_path.as_path(), serde_json::to_string_pretty(&session)?)?;
                                    Ok(())
                                }
                            }),
                        )
                        .expect("session callback can only be set once");

                    if generation_arc.load(Ordering::SeqCst) != generation {
                        return;
                    }

                    match client
                        .matrix_auth()
                        .login_sso(|sso_url| async move {
                            if let Err(err) = open::that(&sso_url) {
                                eprintln!("Failed to open SSO browser: {}", err);
                                return Err(matrix_sdk::Error::Io(err));
                            }
                            Ok(())
                        })
                        .initial_device_display_name("Avernus GPUI")
                        .await
                    {
                        Ok(_) => {
                            if !Self::ensure_cross_signing_bootstrapped(&client).await {
                                let _ = tx
                                    .send(MatrixBridgeEvent::ConnectionFailed {
                                        generation,
                                        error: "Matrix device trust state was stale; please log in again to reset the encryption session."
                                            .to_string(),
                                    })
                                    .await;
                                return;
                            }

                            if let Err(error) = Self::persist_session(&client) {
                                eprintln!("Failed to persist Matrix session after SSO login: {error}");
                            }

                            let client_for_sync = client.clone();
                            let tx_for_sync = tx.clone();
                            {
                                let mut lock = client_arc.write().await;
                                *lock = Some(client.clone());
                            }

                            client.add_event_handler({
                                let tx = tx.clone();
                                let generation = generation;
                                move |event: SyncRoomMessageEvent, room: Room| async move {
                                    let Some(original) = (match event {
                                        SyncRoomMessageEvent::Original(original) => Some(original),
                                        SyncRoomMessageEvent::Redacted(_) => None,
                                    }) else {
                                        return;
                                    };

                                    if let MessageType::Text(text_content) = original.content.msgtype {
                                        let msg = MatrixMessage {
                                            room_id: room.room_id().to_string(),
                                            sender: original.sender.to_string(),
                                            body: text_content.body.clone(),
                                            timestamp: original.origin_server_ts.get().into(),
                                            event_id: Some(original.event_id.to_string()),
                                        };
                                        let _ = tx.send(MatrixBridgeEvent::NewMessage { generation, message: msg }).await;
                                    }
                                }
                            });

                            if generation_arc.load(Ordering::SeqCst) != generation {
                                return;
                            }

                            let initial_rooms = Self::collect_joined_rooms(&client_for_sync).await;
                            if generation_arc.load(Ordering::SeqCst) != generation {
                                return;
                            }
                            let _ = tx_for_sync
                                .send(MatrixBridgeEvent::SyncRooms { generation, rooms: initial_rooms })
                                .await;
                            let _ = tx.send(MatrixBridgeEvent::Connected { generation }).await;

                            let mut sync_settings = SyncSettings::default();
                            loop {
                                if generation_arc.load(Ordering::SeqCst) != generation {
                                    return;
                                }
                                match client_for_sync.sync_once(sync_settings.clone()).await {
                                    Ok(response) => {
                                        sync_settings = sync_settings.token(response.next_batch);

                                        let mut rooms = Vec::new();
                                        for room in client_for_sync.joined_rooms() {
                                            let display_name = room
                                                .display_name()
                                                .await
                                                .map(|name| name.to_string())
                                                .unwrap_or_else(|_| room.room_id().to_string());

                                            let is_direct = room.is_direct().await.unwrap_or(false);
                                            let room_id = room.room_id().to_string();

                                            rooms.push(RoomInfo {
                                                id: room_id,
                                                display_name,
                                                is_direct,
                                            });
                                        }

                                        if generation_arc.load(Ordering::SeqCst) != generation {
                                            return;
                                        }
                                        let _ = tx_for_sync.send(MatrixBridgeEvent::SyncRooms { generation, rooms }).await;
                                    }
                                    Err(error) => {
                                        eprintln!("Matrix Sync Error: {error}");
                                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            let _ = tx
                                .send(MatrixBridgeEvent::ConnectionFailed {
                                    generation,
                                    error: error.to_string(),
                                })
                                .await;
                        }
                    }
                });
            })
            .detach();
    }

    pub fn create_direct_chat(&mut self, cx: &mut Context<Self>, user_id_str: String) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let generation = self.sync_generation.load(Ordering::SeqCst);
        let rt_handle = tokio::runtime::Handle::current();

        cx.background_executor()
            .spawn(async move {
                rt_handle.spawn(async move {
                    let client = {
                        let lock = client_arc.read().await;
                        lock.clone()
                    };

                    let Some(client) = client else {
                        eprintln!("Client not initialized");
                        return;
                    };

                    let Ok(user_id) = UserId::parse(&user_id_str) else {
                        eprintln!("Invalid User ID: {}", user_id_str);
                        return;
                    };

                    let request = assign!(CreateRoomRequest::new(), {
                        is_direct: true,
                        invite: vec![user_id],
                    });

                    match client.create_room(request).await {
                        Ok(room) => {
                            println!("Created direct chat with room ID: {}", room.room_id());
                            Self::refresh_joined_rooms(&client, &tx, generation).await;
                        }
                        Err(err) => eprintln!("Failed to create direct chat: {}", err),
                    }
                });
            })
            .detach();
    }

    pub fn create_room(&mut self, cx: &mut Context<Self>, room_name: String) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let generation = self.sync_generation.load(Ordering::SeqCst);
        let rt_handle = tokio::runtime::Handle::current();

        cx.background_executor()
            .spawn(async move {
                rt_handle.spawn(async move {
                    let client = {
                        let lock = client_arc.read().await;
                        lock.clone()
                    };

                    let Some(client) = client else {
                        eprintln!("Client not initialized");
                        return;
                    };

                    let request = assign!(CreateRoomRequest::new(), {
                        name: Some(room_name),
                    });

                    match client.create_room(request).await {
                        Ok(room) => {
                            println!("Created room ID: {}", room.room_id());
                            Self::refresh_joined_rooms(&client, &tx, generation).await;
                        }
                        Err(err) => eprintln!("Failed to create room: {}", err),
                    }
                });
            })
            .detach();
    }

    pub fn search_public_rooms(&self, cx: &mut Context<Self>, query: String) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let rt_handle = tokio::runtime::Handle::current();
        let generation = self.sync_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation_arc = Arc::clone(&self.sync_generation);

        cx.background_executor().spawn(async move {
            rt_handle.spawn(async move {
                let client = {
                    let lock = client_arc.read().await;
                    lock.clone()
                };

                let Some(client) = client else {
                    eprintln!("Client not initialized");
                    return;
                };

                let trimmed = query.trim().to_string();
                if trimmed.is_empty() {
                    let _ = tx.send(MatrixBridgeEvent::PublicRoomsDiscovered { generation, rooms: Vec::new() }).await;
                    return;
                }

                let mut search = matrix_sdk::room_directory_search::RoomDirectorySearch::new(client);
                if let Err(error) = search.search(Some(trimmed.clone()), 10, None).await {
                    let _ = tx.send(MatrixBridgeEvent::ConnectionFailed { generation, error: error.to_string() }).await;
                    return;
                }

                let (results, _stream) = search.results();
                let discovered = results
                    .iter()
                    .take(12)
                    .map(|room| PublicRoomSearchResult {
                        room_id: room.room_id.to_string(),
                        name: room.name.clone(),
                        topic: room.topic.clone(),
                        alias: room.alias.as_ref().map(|alias| alias.to_string()),
                    })
                    .collect();

                if generation_arc.load(Ordering::SeqCst) != generation {
                    return;
                }

                let _ = tx.send(MatrixBridgeEvent::PublicRoomsDiscovered { generation, rooms: discovered }).await;
            });
        }).detach();
    }

    pub fn join_room_by_id_or_alias(&mut self, cx: &mut Context<Self>, room_id_or_alias: String) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let generation = self.sync_generation.load(Ordering::SeqCst);
        let rt_handle = tokio::runtime::Handle::current();

        cx.background_executor()
            .spawn(async move {
                rt_handle.spawn(async move {
                    let client = {
                        let lock = client_arc.read().await;
                        lock.clone()
                    };

                    let Some(client) = client else {
                        eprintln!("Client not initialized");
                        return;
                    };

                    let input = room_id_or_alias.trim();
                    let normalized = if input.is_empty() {
                        String::new()
                    } else if input.starts_with("https://matrix.to/#/") || input.starts_with("http://matrix.to/#/") {
                        let without_prefix = input
                            .trim_start_matches("https://matrix.to/#/")
                            .trim_start_matches("http://matrix.to/#/");
                        if without_prefix.starts_with("#") || without_prefix.starts_with("!") {
                            without_prefix.to_string()
                        } else {
                            format!("#{}", without_prefix)
                        }
                    } else if input.starts_with("https://") || input.starts_with("http://") {
                        let mut candidate = input;
                        if let Some(hash_pos) = candidate.rfind('#') {
                            candidate = &candidate[hash_pos + 1..];
                        }
                        if candidate.starts_with("#") || candidate.starts_with("!") {
                            candidate.to_string()
                        } else {
                            format!("#{}", candidate)
                        }
                    } else if input.starts_with('#') || input.starts_with('!') {
                        input.to_string()
                    } else if input.contains(':') {
                        format!("#{}", input)
                    } else if input.contains('.') {
                        format!("#{}:matrix.org", input.trim_start_matches('#'))
                    } else {
                        format!("#{}:matrix.org", input.trim_start_matches('#'))
                    };

                    if normalized.is_empty() {
                        eprintln!("Invalid Room ID or Alias: empty input");
                        return;
                    }

                    let Ok(room_or_alias_id) = RoomOrAliasId::parse(&normalized) else {
                        eprintln!("Invalid Room ID or Alias: {}", normalized);
                        return;
                    };

                    match client.join_room_by_id_or_alias(&room_or_alias_id, &[]).await {
                        Ok(room) => {
                            println!("Joined room: {}", room.room_id());
                            Self::refresh_joined_rooms(&client, &tx, generation).await;
                        }
                        Err(err) => eprintln!("Failed to join room: {}", err),
                    }
                });
            })
            .detach();
    }

    pub fn load_room_history(&self, cx: &mut Context<Self>, room_id_str: String) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let rt_handle = tokio::runtime::Handle::current();
        let generation = self.sync_generation.load(Ordering::SeqCst);

        cx.background_executor().spawn(async move {
            rt_handle.spawn(async move {
                let client = {
                    let lock = client_arc.read().await;
                    lock.clone()
                };

                let Some(client) = client else {
                    return;
                };

                let Ok(room_id) = RoomId::parse(&room_id_str) else {
                    return;
                };

                let Some(room) = client.get_room(&room_id) else {
                    return;
                };

                let mut options = matrix_sdk::room::MessagesOptions::backward();
                options.limit = matrix_sdk::ruma::uint!(200);

                let Ok(messages_response) = room.messages(options).await else {
                    return;
                };

                let mut history = Vec::new();
                for event in messages_response.chunk {
                    let Ok(sync_event) = event.kind.raw().deserialize() else {
                        continue;
                    };

                    let Some(original) = (match sync_event {
                        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                            SyncMessageLikeEvent::Original(original),
                        )) => Some(original),
                        _ => None,
                    }) else {
                        continue;
                    };

                    if let MessageType::Text(text_content) = original.content.msgtype {
                        history.push(MatrixMessage {
                            room_id: room_id_str.clone(),
                            sender: original.sender.to_string(),
                            body: text_content.body.clone(),
                            timestamp: original.origin_server_ts.get().into(),
                            event_id: Some(original.event_id.to_string()),
                        });
                    }
                }

                eprintln!("load_room_history: {} -> {} messages", room_id_str, history.len());
                let _ = tx
                    .send(MatrixBridgeEvent::RoomHistory {
                        generation,
                        room_id: room_id_str,
                        messages: history,
                    })
                    .await;
            });
        }).detach();
    }

    pub fn send_message(&mut self, cx: &mut Context<Self>, room_id_str: String, text: String) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let rt_handle = tokio::runtime::Handle::current();
        let generation = self.sync_generation.load(Ordering::SeqCst);

        cx.background_executor()
            .spawn(async move {
                rt_handle.spawn(async move {
                    let client = {
                        let lock = client_arc.read().await;
                        lock.clone()
                    };

                    let Some(client) = client else {
                        eprintln!("Client not initialized");
                        return;
                    };

                    let Ok(room_id) = RoomId::parse(&room_id_str) else {
                        eprintln!("Invalid Room ID: {}", room_id_str);
                        return;
                    };

                    let Some(room) = client.get_room(&room_id) else {
                        eprintln!("Room not found for send: {}", room_id_str);
                        return;
                    };

                    let local_sender = client
                        .user_id()
                        .map(|user_id| user_id.to_string())
                        .unwrap_or_else(|| "you".to_string());

                    let message = MatrixMessage {
                        room_id: room_id_str.clone(),
                        sender: local_sender.clone(),
                        body: text.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        event_id: None,
                    };

                    let _ = tx
                        .send(MatrixBridgeEvent::NewMessage { generation, message: message.clone() })
                        .await;

                    let content = RoomMessageEventContent::text_plain(text);
                    match room.send(content).await {
                        Ok(_) => {}
                        Err(err) => {
                            eprintln!("Failed to send message: {}", err);
                        }
                    }
                });
            })
            .detach();
    }

    pub fn send_attachment(
        &mut self,
        cx: &mut Context<Self>,
        room_id_str: String,
        path: std::path::PathBuf,
        caption: Option<String>,
    ) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let rt_handle = tokio::runtime::Handle::current();
        let generation = self.sync_generation.load(Ordering::SeqCst);
        let filename = Self::attachment_display_name(&path);
        let mime_type = Self::attachment_content_type(&path);

        cx.background_executor()
            .spawn(async move {
                rt_handle.spawn(async move {
                    let client = {
                        let lock = client_arc.read().await;
                        lock.clone()
                    };

                    let Some(client) = client else {
                        eprintln!("Client not initialized");
                        return;
                    };

                    let Ok(room_id) = RoomId::parse(&room_id_str) else {
                        eprintln!("Invalid Room ID: {}", room_id_str);
                        return;
                    };

                    let Some(room) = client.get_room(&room_id) else {
                        eprintln!("Room not found for attachment send: {}", room_id_str);
                        return;
                    };

                    let data = match std::fs::read(&path) {
                        Ok(data) => data,
                        Err(err) => {
                            eprintln!("Failed to read attachment {}: {}", path.display(), err);
                            return;
                        }
                    };

                    let local_sender = client
                        .user_id()
                        .map(|user_id| user_id.to_string())
                        .unwrap_or_else(|| "you".to_string());

                    let body = caption
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .map(|value| format!("Attachment: {} ({})", filename, value))
                        .unwrap_or_else(|| format!("Attachment: {}", filename));

                    let local_message = MatrixMessage {
                        room_id: room_id_str.clone(),
                        sender: local_sender.clone(),
                        body: body.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        event_id: None,
                    };

                    let _ = tx
                        .send(MatrixBridgeEvent::NewMessage { generation, message: local_message })
                        .await;

                    let mut config = matrix_sdk::attachment::AttachmentConfig::new();
                    if let Some(caption_text) = caption.filter(|value| !value.trim().is_empty()) {
                        config = config.caption(Some(TextMessageEventContent::plain(caption_text)));
                    }

                    let file_name_for_error = filename.clone();
                    if let Err(err) = room.send_attachment(filename, &mime_type, data, config).await {
                        eprintln!("Failed to send attachment {}: {}", file_name_for_error, err);
                    }
                });
            })
            .detach();
    }

    pub fn leave_room(&mut self, cx: &mut Context<Self>, room_id: String) {
        self.rooms.retain(|room| room.id != room_id);
        self.messages.remove(&room_id);
        self.sync_generation.fetch_add(1, Ordering::SeqCst);

        let client_arc = Arc::clone(&self.client);
        let rt_handle = tokio::runtime::Handle::current();

        cx.background_executor().spawn(async move {
            rt_handle.spawn(async move {
                let client = {
                    let lock = client_arc.read().await;
                    lock.clone()
                };

                let Some(client) = client else {
                    return;
                };

                let Ok(room_id) = RoomId::parse(&room_id) else {
                    return;
                };

                if let Some(room) = client.get_room(&room_id) {
                    if let Err(err) = room.leave().await {
                        eprintln!("Failed to leave room {}: {}", room_id, err);
                    }
                }
            });
        }).detach();

        cx.notify();
    }

    pub fn confirm_verification_challenge(&mut self, cx: &mut Context<Self>, user_id: String, flow_id: String) {
        self.pending_verification = None;
        let client_arc = Arc::clone(&self.client);
        let rt_handle = tokio::runtime::Handle::current();
        let user_id_for_task = user_id.clone();
        let flow_id_for_task = flow_id.clone();

        cx.background_executor().spawn(async move {
            rt_handle.spawn(async move {
                let client = {
                    let lock = client_arc.read().await;
                    lock.clone()
                };

                let Some(client) = client else {
                    eprintln!("Client not initialized while confirming verification challenge");
                    return;
                };

                let Ok(user_id) = UserId::parse(&user_id_for_task) else {
                    eprintln!("Invalid user ID while confirming verification challenge: {user_id_for_task}");
                    return;
                };

                let Some(sas_verification) = client
                    .encryption()
                    .get_verification(&user_id, &flow_id_for_task)
                    .await
                    .and_then(|verification| verification.sas())
                else {
                    eprintln!("Verification flow not found while confirming challenge: {flow_id_for_task}");
                    return;
                };

                if let Err(error) = sas_verification.confirm().await {
                    eprintln!("Failed to confirm SAS verification challenge: {error}");
                }
            });
        }).detach();
    }

    pub fn mismatch_verification_challenge(&mut self, cx: &mut Context<Self>, user_id: String, flow_id: String) {
        self.pending_verification = None;
        let client_arc = Arc::clone(&self.client);
        let rt_handle = tokio::runtime::Handle::current();
        let user_id_for_task = user_id.clone();
        let flow_id_for_task = flow_id.clone();

        cx.background_executor().spawn(async move {
            rt_handle.spawn(async move {
                let client = {
                    let lock = client_arc.read().await;
                    lock.clone()
                };

                let Some(client) = client else {
                    eprintln!("Client not initialized while rejecting verification challenge");
                    return;
                };

                let Ok(user_id) = UserId::parse(&user_id_for_task) else {
                    eprintln!("Invalid user ID while rejecting verification challenge: {user_id_for_task}");
                    return;
                };

                let Some(sas_verification) = client
                    .encryption()
                    .get_verification(&user_id, &flow_id_for_task)
                    .await
                    .and_then(|verification| verification.sas())
                else {
                    eprintln!("Verification flow not found while rejecting challenge: {flow_id_for_task}");
                    return;
                };

                if let Err(error) = sas_verification.mismatch().await {
                    eprintln!("Failed to reject SAS verification challenge: {error}");
                }
            });
        }).detach();
    }

    pub fn verify_current_session(&self, cx: &mut Context<Self>) {
        let client_arc = Arc::clone(&self.client);
        let tx = self.event_tx.clone();
        let generation = self.sync_generation.load(Ordering::SeqCst);
        let rt_handle = tokio::runtime::Handle::current();

        cx.background_executor().spawn(async move {
            rt_handle.spawn(async move {
                let client = {
                    let lock = client_arc.read().await;
                    lock.clone()
                };

                let Some(client) = client else {
                    eprintln!("Client not initialized while verifying session");
                    return;
                };

                if !client.matrix_auth().logged_in() {
                    eprintln!("Cannot verify device: Matrix client is not logged in");
                    return;
                }

                let Some(user_id) = client.user_id() else {
                    eprintln!("Cannot verify session: the Matrix user ID is not available.");
                    return;
                };

                let current_device_id = match client.device_id() {
                    Some(device_id) => device_id.to_owned(),
                    None => {
                        eprintln!("Cannot verify session: the Matrix device ID is not available.");
                        return;
                    }
                };

                match client.encryption().get_user_devices(user_id).await {
                    Ok(devices) => {
                        let target_device = devices
                            .devices()
                            .find(|device| device.device_id() != current_device_id);

                        match target_device {
                            Some(device) => {
                                match device.request_verification().await {
                                    Ok(verification) => {
                                        let request_user_id = verification.other_user_id().to_string();
                                        let request_flow_id = verification.flow_id().to_string();
                                        eprintln!(
                                            "Verification request sent to device {} for user {}. Accept it from the other trusted client/session.",
                                            device.device_id(),
                                            request_user_id
                                        );

                                        if let Ok(Some(sas_verification)) = verification.start_sas().await {
                                            Self::wait_for_sas_challenge(
                                                &tx,
                                                generation,
                                                &request_user_id,
                                                &request_flow_id,
                                                sas_verification,
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        eprintln!("Failed to send a valid verification request to the peer device: {error}");
                                    }
                                }
                            }
                            None => {
                                eprintln!(
                                    "No other Matrix device is available for verification. Open an existing trusted client/session and complete the verification there; Avernus will not send self-verification requests."
                                );
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("Failed to load other Matrix devices for verification: {error}");
                    }
                }
            });
        }).detach();
    }

    pub fn reset_stale_encryption_session(&mut self, cx: &mut Context<Self>) {
        self.sync_generation.fetch_add(1, Ordering::SeqCst);
        Self::clear_session_state();
        self.client = Arc::new(RwLock::new(None));
        self.rooms.clear();
        self.messages.clear();
        self.pending_verification = None;
        self.connection_status = "Offline".to_string();
        cx.notify();
    }

    pub fn logout(&mut self, cx: &mut Context<Self>) {
        self.sync_generation.fetch_add(1, Ordering::SeqCst);
        Self::clear_session_state();
        self.client = Arc::new(RwLock::new(None));
        self.rooms.clear();
        self.messages.clear();
        self.pending_verification = None;
        self.connection_status = "Offline".to_string();
        cx.notify();
    }

    async fn ensure_cross_signing_bootstrapped(client: &Client) -> bool {
        if !client.matrix_auth().logged_in() {
            return true;
        }

        let status = client.encryption().cross_signing_status().await;
        if matches!(status, Some(ref status) if status.is_complete()) {
            return true;
        }

        match client.encryption().bootstrap_cross_signing_if_needed(None).await {
            Ok(_) => true,
            Err(error) => {
                eprintln!("Failed to bootstrap cross-signing for Matrix device: {error}");
                let _ = client.logout().await;
                Self::clear_session_state();
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MatrixBackend, MatrixMessage, PendingVerificationChallenge, VerificationEmoji};
    use matrix_sdk::{ruma::ServerName, Client};
    use matrix_sdk::ruma::events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
    };
    use std::path::PathBuf;

    #[tokio::test]
    async fn ensure_cross_signing_bootstrapped_skips_logged_out_clients() {
        let server_name = ServerName::parse("matrix.org").expect("valid Matrix server");
        let client = Client::builder()
            .server_name(&server_name)
            .build()
            .await
            .expect("client should build without a live session");

        assert!(MatrixBackend::ensure_cross_signing_bootstrapped(&client).await);
    }

    #[test]
    fn verification_challenge_tracks_emoji_data() {
        let challenge = PendingVerificationChallenge {
            user_id: "@alice:example.org".to_string(),
            flow_id: "flow-123".to_string(),
            emojis: vec![VerificationEmoji {
                symbol: "🦊".to_string(),
                description: "Fox".to_string(),
            }],
        };

        assert_eq!(challenge.user_id, "@alice:example.org");
        assert_eq!(challenge.flow_id, "flow-123");
        assert_eq!(challenge.emojis.len(), 1);
        assert_eq!(challenge.emojis[0].description, "Fox");
    }

    #[test]
    fn room_history_merge_deduplicates_repeated_messages() {
        let room_id = "!room:example.org".to_string();
        let message = MatrixMessage {
            room_id: room_id.clone(),
            sender: "@alice:example.org".to_string(),
            body: "hello".to_string(),
            timestamp: 123,
            event_id: None,
        };

        let mut existing = vec![message.clone()];
        MatrixBackend::merge_room_history(&mut existing, vec![message.clone(), message.clone()]);

        assert_eq!(existing.len(), 1);
        assert_eq!(existing[0].body, "hello");
    }

    #[test]
    fn local_echo_duplicates_are_deduplicated_within_a_short_time_window() {
        let room_id = "!room:example.org".to_string();
        let first = MatrixMessage {
            room_id: room_id.clone(),
            sender: "@alice:example.org".to_string(),
            body: "s".to_string(),
            timestamp: 100,
            event_id: None,
        };
        let second = MatrixMessage {
            room_id: room_id.clone(),
            sender: "@alice:example.org".to_string(),
            body: "s".to_string(),
            timestamp: 105,
            event_id: None,
        };
        let third = MatrixMessage {
            room_id: room_id.clone(),
            sender: "@alice:example.org".to_string(),
            body: "s".to_string(),
            timestamp: 10_000,
            event_id: None,
        };

        assert!(MatrixBackend::is_same_message(&first, &second));
        assert!(!MatrixBackend::is_same_message(&first, &third));
    }

    #[test]
    fn attachment_content_type_guesses_known_media_types() {
        assert_eq!(
            MatrixBackend::attachment_content_type(PathBuf::from("photo.png").as_path()),
            mime::IMAGE_PNG
        );
        assert_eq!(
            MatrixBackend::attachment_content_type(PathBuf::from("report.pdf").as_path()),
            mime::APPLICATION_PDF
        );
    }

    #[test]
    fn parses_room_sync_message_events() {
        let json = r#"{
            "content": {"msgtype": "m.text", "body": "hello"},
            "event_id": "$event:example.org",
            "origin_server_ts": 1,
            "sender": "@alice:example.org",
            "type": "m.room.message"
        }"#;

        let event: AnySyncTimelineEvent =
            serde_json::from_str(json).expect("sync timeline event should parse");

        assert!(matches!(
            event,
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncMessageLikeEvent::Original(_)
            ))
        ));
    }
}