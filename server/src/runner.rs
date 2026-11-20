use super::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Notify, mpsc};

const FRAME_LIMIT: usize = 8 * 1024 * 1024;
const LEASE: Duration = Duration::from_secs(60);
const MAX_BUFFERED_FRAMES: usize = 1024;

pub(super) struct Runner {
    id: Uuid,
    token: [u8; 32],
    state: Mutex<StateData>,
    notify: Notify,
    version: AtomicU64,
}

#[derive(Default)]
struct StateData {
    connection: Option<(Uuid, mpsc::Sender<Value>)>,
    roots: Vec<String>,
    host_info: Value,
    requests: HashMap<Uuid, Request>,
}

struct Request {
    session: String,
    project: Uuid,
    root: String,
    operation: String,
    state: &'static str,
    last_poll: Instant,
    frames: VecDeque<(i64, usize, Value)>,
    sequence: i64,
    bytes: usize,
    input_ids: HashSet<Uuid>,
}

impl Runner {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn new(id: Uuid, secret: &str) -> Self {
        Self {
            id,
            token: Sha256::digest(secret.as_bytes()).into(),
            state: Mutex::default(),
            notify: Notify::new(),
            version: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StateData> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn signal(&self) {
        self.version.fetch_add(1, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Race-free wait: registers the waiter before checking `version`, eliminating lost wakeups.
    pub async fn wait_since(&self, seen: u64, timeout: Duration) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.version.load(Ordering::Acquire) != seen {
            return;
        }
        let _ = tokio::time::timeout(timeout, notified).await;
    }

    pub fn inventory(&self) -> Value {
        let state = self.lock();
        json!({"items":[{"id":self.id,"name":"Venticode","roots":state.roots,
            "online":state.connection.is_some(),"mode":"sandboxed","owned":true,"revision":1,"host_info":state.host_info}]})
    }

    pub fn online(&self) -> bool {
        self.lock().connection.is_some()
    }

    pub fn has_request(&self, id: Uuid) -> bool {
        self.lock().requests.contains_key(&id)
    }

    pub fn forget(&self, id: Uuid) {
        self.lock().requests.remove(&id);
    }

    /// Renew an active request's poll lease while the server performs slow work (e.g. subcompletion).
    pub fn touch_request(&self, id: Uuid) {
        let mut state = self.lock();
        if let Some(req) = state.requests.get_mut(&id) {
            req.last_poll = Instant::now();
        }
    }

    pub fn respond(&self, id: Uuid, frame: Value) -> Result<()> {
        let mut state = self.lock();
        let Some(req) = state.requests.get_mut(&id).filter(|r| r.state == "sent") else {
            return Err(missing());
        };
        req.last_poll = Instant::now();
        state
            .connection
            .as_ref()
            .ok_or_else(unavailable)?
            .1
            .try_send(frame)
            .map_err(|_| unavailable())
    }

    pub async fn load_inventory(
        &self,
        db: &PgPool,
        workspace: Uuid,
    ) -> std::result::Result<(), sqlx::Error> {
        let row: Option<(sqlx::types::Json<Vec<String>>, Value)> = sqlx::query_as(
            "SELECT roots,host_info FROM standalone_runner_inventory WHERE workspace=$1 AND runner_id=$2",
        )
        .bind(workspace)
        .bind(self.id)
        .fetch_optional(db)
        .await?;
        if let Some((roots, host_info)) = row {
            let mut state = self.lock();
            state.roots = roots.0;
            state.host_info = host_info;
        }
        Ok(())
    }

    pub fn action(&self, session: String, body: &Value) -> Result<Value> {
        let mut state = self.lock();
        state
            .requests
            .retain(|_, r| r.state == "sent" || r.last_poll.elapsed() < LEASE);
        let id = uuid(body, "request_id")?;
        let sender = state.connection.as_ref().map(|(_, tx)| tx.clone());
        match body["action"].as_str() {
            Some("execute") => {
                let sender = sender.ok_or_else(unavailable)?;
                let root = body["root"].as_str().ok_or_else(bad)?;
                if uuid(body, "harness_id")? != self.id
                    || !state.roots.iter().any(|label| label == root)
                {
                    return Err(missing());
                }
                let operation = body["operation"].as_str().ok_or_else(bad)?;
                let path = body["project_path"].as_str().ok_or_else(bad)?;
                if !crate::projects::relative(path)
                    || !(crate::tools::is_project_operation(operation)
                        || crate::tools::is_import_operation(operation)
                        || body["managed"] == true && crate::computer::is_operation(operation))
                {
                    return Err(bad());
                }
                if state.requests.contains_key(&id) {
                    return Err(conflict());
                }
                if state
                    .requests
                    .values()
                    .filter(|r| r.state == "sent")
                    .count()
                    >= 24
                    || state.requests.len() >= 512
                {
                    return Err(unavailable());
                }
                let project = uuid(body, "project_id")?;
                sender
                    .try_send(json!({"type":"execute","id":id,"payload":{
                        "consent_id":id,"root":root,"mode":"sandboxed","operation":"project_tool","mcp_grants":{},
                        "arguments":{"project_path":path,"operation":operation,"arguments":body["arguments"],"include_diff":body["include_diff"] == true}
                    }}))
                    .map_err(|_| unavailable())?;
                state.requests.insert(
                    id,
                    Request {
                        session,
                        project,
                        root: root.into(),
                        operation: operation.into(),
                        state: "sent",
                        last_poll: Instant::now(),
                        frames: VecDeque::new(),
                        sequence: 0,
                        bytes: 0,
                        input_ids: HashSet::new(),
                    },
                );
                Ok(json!({"accepted":true}))
            }
            Some("poll" | "cancel") => {
                let request = state
                    .requests
                    .get_mut(&id)
                    .filter(|r| r.session == session)
                    .ok_or_else(missing)?;
                if body["action"] == "cancel" {
                    if request.state == "sent" {
                        if let Some(sender) = sender {
                            sender
                                .try_send(json!({"type":"cancel","id":id}))
                                .map_err(|_| unavailable())?;
                        }
                        request.state = "cancelled";
                        request.frames.clear();
                        request.bytes = 0;
                    }
                    return Ok(json!({"state":request.state}));
                }
                let after = body["after"]
                    .as_i64()
                    .filter(|n| *n >= 0 && *n <= request.sequence)
                    .ok_or_else(bad)?;
                request.last_poll = Instant::now();
                while request
                    .frames
                    .front()
                    .is_some_and(|(seq, _, _)| *seq <= after)
                {
                    if let Some((_, size, _)) = request.frames.pop_front() {
                        request.bytes -= size;
                    }
                }
                let cursor = request
                    .frames
                    .iter()
                    .take(32)
                    .next_back()
                    .map_or(after, |(seq, _, _)| *seq);
                let frames = request
                    .frames
                    .iter()
                    .take(32)
                    .map(|(_, _, frame)| frame)
                    .collect::<Vec<_>>();
                Ok(json!({"state":request.state,"frames":frames,"after":cursor}))
            }
            Some("input") => {
                let sender = sender.ok_or_else(unavailable)?;
                let parent = uuid(body, "session_id")?;
                let project = uuid(body, "project_id")?;
                let request = state
                    .requests
                    .get_mut(&parent)
                    .filter(|r| r.session == session && r.project == project && r.state == "sent")
                    .ok_or_else(forbidden)?;
                if request.input_ids.len() >= 1024 || request.input_ids.contains(&id) {
                    return Err(conflict());
                }
                let text = body["text"].as_str().unwrap_or_default();
                if text.len() > 8192 {
                    return Err(bad());
                }
                let (operation, args) = match (
                    request.operation.as_str(),
                    body["input_action"].as_str(),
                ) {
                    ("terminal_start", Some("send")) if !text.is_empty() => (
                        "terminal_input",
                        json!({"session_id":parent,"data":STANDARD.encode(format!("{text}\r"))}),
                    ),
                    ("agent_start", Some("send")) if !text.is_empty() => {
                        ("agent_prompt", json!({"session_id":parent,"prompt":text}))
                    }
                    ("terminal_start", Some("stop")) => {
                        ("terminal_stop", json!({"session_id":parent}))
                    }
                    ("agent_start", Some("stop")) => ("agent_stop", json!({"session_id":parent})),
                    _ => return Err(bad()),
                };
                sender
                    .try_send(json!({"type":"input","id":id,"payload":{"consent_id":parent,"root":request.root,"mode":"sandboxed","operation":operation,"arguments":args,"mcp_grants":{}}}))
                    .map_err(|_| unavailable())?;
                request.input_ids.insert(id);
                Ok(json!({"accepted":true}))
            }
            _ => Err(bad()),
        }
    }

    fn expire(&self, connection: Uuid) -> Vec<Uuid> {
        let mut state = self.lock();
        if state
            .connection
            .as_ref()
            .is_none_or(|(id, _)| *id != connection)
        {
            return Vec::new();
        }
        let mut cancel = Vec::new();
        state.requests.retain(|id, r| {
            if r.last_poll.elapsed() < LEASE {
                return true;
            }
            if r.state == "sent" {
                cancel.push(*id);
            }
            false
        });
        cancel
    }

    fn receive(&self, connection: Uuid, size: usize, frame: Value) -> Option<Uuid> {
        let id = uuid(&frame, "id").ok()?;
        let mut state = self.lock();
        if state
            .connection
            .as_ref()
            .is_none_or(|(id, _)| *id != connection)
        {
            return None;
        }
        let r = state.requests.get_mut(&id).filter(|r| r.state == "sent")?;
        match frame["type"].as_str() {
            Some("complete") => r.state = "done",
            Some("output") => {}
            _ => return None,
        }
        if r.frames.len() >= MAX_BUFFERED_FRAMES || r.bytes + size > 16 * 1024 * 1024 {
            tracing::warn!(
                runner_id = %self.id,
                request_id = %id,
                buffered_frames = r.frames.len(),
                buffered_bytes = r.bytes,
                "runner request exceeded buffer limit, cancelling"
            );
            r.state = "failed";
            r.frames.clear();
            r.bytes = 0;
            self.signal();
            return Some(id);
        }
        r.sequence += 1;
        r.bytes += size;
        r.frames.push_back((r.sequence, size, frame));
        self.signal();
        None
    }
}

fn uuid(value: &Value, field: &str) -> Result<Uuid> {
    value[field]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(bad)
}

pub(super) async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    let local = app.standalone.ok_or_else(missing)?;
    local.validate_host(&headers, false)?;
    let secret = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(forbidden)?;
    if !bool::from(
        local
            .runner
            .token
            .ct_eq(Sha256::digest(secret.as_bytes()).as_slice()),
    ) {
        return Err(forbidden());
    }
    Ok(ws
        .max_message_size(FRAME_LIMIT)
        .max_frame_size(FRAME_LIMIT)
        .on_upgrade(move |socket| async move {
            serve(&local.runner, &app.db, local.workspace, socket).await
        }))
}

pub(super) async fn serve(runner: &Runner, db: &PgPool, workspace: Uuid, mut socket: WebSocket) {
    let text = match tokio::time::timeout(Duration::from_secs(5), socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => text,
        Ok(other) => {
            tracing::warn!(runner_id = %runner.id, frame = ?other.map(|r| r.is_ok()), "runner handshake failed before inventory");
            return;
        }
        Err(_) => {
            tracing::warn!(runner_id = %runner.id, "runner handshake timed out waiting for inventory");
            return;
        }
    };
    #[derive(Deserialize)]
    struct Inventory {
        #[serde(rename = "type")]
        kind: String,
        roots: Vec<String>,
        host_info: Value,
    }
    let Ok(inventory) = serde_json::from_str::<Inventory>(&text) else {
        tracing::warn!(runner_id = %runner.id, "runner sent malformed inventory JSON");
        return;
    };
    if inventory.kind != "inventory"
        || inventory.roots.is_empty()
        || inventory.roots.len() > 32
        || inventory.roots.iter().any(|s| {
            s.is_empty()
                || s.len() > 80
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        tracing::warn!(runner_id = %runner.id, "runner inventory rejected due to invalid roots");
        return;
    }
    if ["os", "arch", "version"].iter().any(|k| {
        !inventory.host_info[*k]
            .as_str()
            .is_some_and(|v| !v.is_empty() && v.len() <= 80)
    }) {
        tracing::warn!(runner_id = %runner.id, "runner inventory rejected due to invalid host_info");
        return;
    }
    if let Err(err) = sqlx::query(
        "INSERT INTO standalone_runner_inventory(workspace,runner_id,roots,host_info) VALUES($1,$2,$3,$4)
        ON CONFLICT(workspace,runner_id) DO UPDATE SET roots=excluded.roots,host_info=excluded.host_info",
    )
    .bind(workspace)
    .bind(runner.id)
    .bind(json!(inventory.roots))
    .bind(&inventory.host_info)
    .execute(db)
    .await
    {
        tracing::warn!(runner_id = %runner.id, error = %err, "failed to persist runner inventory");
        return;
    }
    let (tx, mut rx) = mpsc::channel::<Value>(128);
    let connection = Uuid::new_v4();
    let root_count = inventory.roots.len();
    {
        let mut state = runner.lock();
        for request in state.requests.values_mut().filter(|r| r.state == "sent") {
            request.state = "failed";
            request.frames.clear();
            request.bytes = 0;
        }
        state.connection = Some((connection, tx.clone()));
        state.roots = inventory.roots;
        state.host_info = inventory.host_info;
    }
    runner.signal();
    tracing::info!(runner_id = %runner.id, roots = root_count, "runner connected");
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut heard = Instant::now();
    let reason;
    loop {
        tokio::select! {
            frame = rx.recv() => {
                let Some(frame) = frame else { reason = "channel_closed"; break; };
                if send(&mut socket, Message::Text(frame.to_string().into())).await.is_err() {
                    reason = "send_failed";
                    break;
                }
            }
            frame = socket.recv() => {
                heard = Instant::now();
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(frame) = serde_json::from_str(&text) else {
                            reason = "malformed_frame";
                            break;
                        };
                        if let Some(id) = runner.receive(connection, text.len(), frame) {
                            let _ = tx.try_send(json!({"type":"cancel","id":id}));
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                    Some(Ok(Message::Close(_))) => { reason = "peer_close"; break; }
                    Some(Err(_)) => { reason = "recv_error"; break; }
                    None => { reason = "eof"; break; }
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                if heard.elapsed() >= Duration::from_secs(20) {
                    reason = "heartbeat_timeout";
                    break;
                }
                if send(&mut socket, Message::Ping(Default::default())).await.is_err() {
                    reason = "ping_failed";
                    break;
                }
                for id in runner.expire(connection) {
                    let _ = tx.try_send(json!({"type":"cancel","id":id}));
                }
            }
        }
    }
    let mut state = runner.lock();
    if state
        .connection
        .as_ref()
        .is_some_and(|(id, _)| *id == connection)
    {
        state.connection = None;
        for request in state.requests.values_mut().filter(|r| r.state == "sent") {
            request.state = "failed";
            request.frames.clear();
            request.bytes = 0;
        }
        runner.signal();
        tracing::info!(runner_id = %runner.id, reason, "runner disconnected");
    }
}

async fn send(socket: &mut WebSocket, message: Message) -> std::result::Result<(), ()> {
    tokio::time::timeout(Duration::from_secs(5), socket.send(message))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_input_and_cancellation_are_bound_to_the_request_owner_and_project() {
        let (tx, mut rx) = mpsc::channel(8);
        let runner = Runner::new(Uuid::new_v4(), "fixture");
        {
            let mut state = runner.state.lock().unwrap();
            state.connection = Some((Uuid::new_v4(), tx));
            state.roots.push("work".into());
        }
        let (id, project) = (Uuid::new_v4(), Uuid::new_v4());
        let mut execute = json!({"action":"execute","request_id":id,"project_id":project,"harness_id":runner.id,
            "root":"work","project_path":"../outside","operation":"terminal_start","arguments":{}});
        assert_eq!(
            runner.action("owner".into(), &execute).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        execute["project_path"] = json!("subfolder");
        runner.action("owner".into(), &execute).unwrap();
        let dispatched = rx.try_recv().unwrap();
        assert_eq!(dispatched["payload"]["consent_id"], id.to_string());
        assert_eq!(
            dispatched["payload"]["arguments"]["project_path"],
            "subfolder"
        );

        let mut input = json!({"action":"input","request_id":Uuid::new_v4(),"project_id":Uuid::new_v4(),
            "session_id":id,"input_action":"send","text":"echo test"});
        assert_eq!(
            runner.action("owner".into(), &input).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        input["project_id"] = json!(project);
        assert_eq!(
            runner
                .action("another session".into(), &input)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        runner.action("owner".into(), &input).unwrap();
        assert_eq!(
            rx.try_recv().unwrap()["payload"]["operation"],
            "terminal_input"
        );
        assert_eq!(
            runner.action("owner".into(), &input).unwrap_err().0,
            StatusCode::CONFLICT
        );

        let cancel = json!({"action":"cancel","request_id":id});
        assert_eq!(
            runner
                .action("another session".into(), &cancel)
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            runner.action("owner".into(), &cancel).unwrap()["state"],
            "cancelled"
        );
        assert_eq!(rx.try_recv().unwrap(), json!({"type":"cancel","id":id}));
    }

    #[tokio::test]
    async fn wait_since_does_not_lose_wakeup() {
        let runner = Runner::new(Uuid::new_v4(), "fixture");
        let v0 = runner.version();
        runner.signal();
        let start = Instant::now();
        runner.wait_since(v0, Duration::from_secs(5)).await;
        assert!(start.elapsed() < Duration::from_millis(200));
    }
}
