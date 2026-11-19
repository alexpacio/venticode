use super::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{collections::HashMap, sync::Mutex, time::Duration};

const AUDIENCE: &str = "venticode-computer";
const API_IDENTITY: &str = "/var/run/secrets/venticode-kubernetes";
const DESKTOP: &str = "desktop";
pub(super) struct Computers {
    kube: reqwest::Client,
    namespace: String,
    machine: Option<MachineConfig>,
    pub kitesurf: Option<String>,
    pub origins: Vec<String>,
    runners: Mutex<HashMap<Uuid, Arc<runner::Runner>>>,
    displays: Mutex<HashMap<Uuid, Arc<tokio::sync::RwLock<()>>>>,
}

struct MachineConfig {
    image: String,
    runtime_class: String,
    storage_class: String,
    origin: String,
    seccomp: String,
}

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/computer", get(status))
        .route("/computer/screen", get(screen))
}

fn image(key: &str) -> std::result::Result<Option<String>, String> {
    let Ok(value) = env::var(key) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let valid = value.rsplit_once("@sha256:").is_some_and(|(name, hash)| {
        !name.is_empty()
            && hash.len() == 64
            && hash
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    });
    if !valid {
        return Err(format!("Use a digest-pinned image for {key}"));
    }
    Ok(Some(value))
}

impl Computers {
    pub fn from_env() -> std::result::Result<Option<Self>, Box<dyn std::error::Error>> {
        let computer = image("VENTICODE_COMPUTER_IMAGE")?;
        let kitesurf = image("VENTICODE_KITESURF_IMAGE")?;
        if computer.is_none() && kitesurf.is_none() {
            return Ok(None);
        }
        let namespace = env::var("VENTICODE_COMPUTER_NAMESPACE")?;
        if namespace.is_empty()
            || namespace.len() > 63
            || !namespace
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err("VENTICODE_COMPUTER_NAMESPACE must name a Kubernetes namespace".into());
        }
        let ca = std::fs::read(format!("{API_IDENTITY}/ca.crt"))?;
        let kube = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&ca)?)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(8))
            .build()?;
        let machine = if let Some(image) = computer {
            let origin = env::var("VENTICODE_COMPUTER_ORIGIN")?;
            let url = reqwest::Url::parse(&origin)?;
            if url.origin().ascii_serialization() != origin
                || !matches!(url.scheme(), "http" | "https")
                || (url.scheme() == "http"
                    && !url.host_str().is_some_and(|host| host.ends_with(".svc")))
            {
                return Err(
                    "Use an HTTPS origin or an in-cluster .svc HTTP origin for the computer relay"
                        .into(),
                );
            }
            Some(MachineConfig {
                image,
                origin,
                runtime_class: env::var("VENTICODE_COMPUTER_RUNTIME_CLASS")?,
                storage_class: env::var("VENTICODE_COMPUTER_STORAGE_CLASS")?,
                seccomp: env::var("VENTICODE_COMPUTER_SECCOMP")?,
            })
        } else {
            None
        };
        let origins: Vec<String> = serde_json::from_str(
            &env::var("VENTICODE_BROWSER_ORIGINS").unwrap_or_else(|_| "[]".into()),
        )?;
        for origin in &origins {
            let url = reqwest::Url::parse(origin)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.origin().ascii_serialization() != *origin
            {
                return Err("List exact HTTP or HTTPS origins in VENTICODE_BROWSER_ORIGINS".into());
            }
        }
        Ok(Some(Self {
            kube,
            namespace,
            machine,
            kitesurf,
            origins,
            runners: Mutex::default(),
            displays: Mutex::default(),
        }))
    }

    pub fn runtimes(&self) -> Vec<&str> {
        let mut modes = vec!["project"];
        if self.kitesurf.is_some() {
            modes.push("kitesurf");
        }
        if self.machine.is_some() {
            modes.push("computer");
        }
        modes
    }

    pub fn runner(&self, id: Uuid) -> Option<Arc<runner::Runner>> {
        self.runners.lock().unwrap().get(&id).cloned()
    }

    pub fn request_runner(&self, id: Uuid) -> Option<Arc<runner::Runner>> {
        self.runners
            .lock()
            .unwrap()
            .values()
            .find(|runner| runner.has_request(id))
            .cloned()
    }

    pub fn display(&self, id: Uuid) -> Arc<tokio::sync::RwLock<()>> {
        self.displays.lock().unwrap().entry(id).or_default().clone()
    }

    pub async fn inventory(
        &self,
        app: &App,
        workspace: Uuid,
        owner: Uuid,
    ) -> Result<Option<Value>> {
        if self.machine.is_none() {
            return Ok(None);
        }
        let Some(id) = self.id(app, workspace, owner).await? else {
            return Ok(None);
        };
        let mut item = self
            .runner(id)
            .map(|runner| runner.inventory()["items"][0].clone())
            .unwrap_or_else(
                || json!({"id":id,"online":false,"mode":"sandboxed","owned":true,"revision":1}),
            );
        item["name"] = json!("Ubuntu");
        item["roots"] = json!(["work"]);
        item["managed"] = json!(true);
        item["model_access"] =
            json!(item["online"] == true && item["host_info"]["model_access"] == true);
        if let Some(host) = item["host_info"].as_object_mut() {
            host.remove("model_access");
        }
        Ok(Some(item))
    }

    async fn id(&self, app: &App, workspace: Uuid, owner: Uuid) -> Result<Option<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM managed_computers WHERE workspace=$1 AND owner_id=$2",
        )
        .bind(workspace)
        .bind(owner)
        .fetch_optional(&app.db)
        .await?)
    }

    pub async fn request(&self, method: Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let token = tokio::fs::read_to_string(format!("{API_IDENTITY}/token"))
            .await
            .map_err(|_| unavailable())?;
        Ok(self
            .kube
            .request(method, format!("https://kubernetes.default.svc{path}"))
            .bearer_auth(token.trim()))
    }

    pub fn namespaced(&self, path: &str) -> String {
        format!("/api/v1/namespaces/{}/{path}", self.namespace)
    }

    pub async fn create(&self, kind: &str, body: Value) -> Result<()> {
        let response = self
            .request(Method::POST, &self.namespaced(kind))
            .await?
            .json(&body)
            .send()
            .await
            .map_err(|_| unavailable())?;
        if response.status().is_success() || response.status() == StatusCode::CONFLICT {
            return Ok(());
        }
        eprintln!(
            "Kubernetes denied creation of {kind}: {}",
            response.status()
        );
        Err(unavailable())
    }

    pub async fn pod(&self, name: &str) -> Result<Option<Value>> {
        let response = self
            .request(Method::GET, &self.namespaced(&format!("pods/{name}")))
            .await?
            .send()
            .await
            .map_err(|_| unavailable())?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response
            .error_for_status()
            .map_err(|_| unavailable())?
            .json()
            .await
            .map_err(|_| unavailable())
    }

    pub async fn delete(&self, path: &str, uid: Option<&str>) -> Result<bool> {
        let mut req = self.request(Method::DELETE, &self.namespaced(path)).await?;
        if let Some(uid) = uid {
            req = req.json(&json!({"preconditions":{"uid":uid}}));
        }
        let response = req.send().await.map_err(|_| unavailable())?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(true),
            s if s.is_success() || s == StatusCode::CONFLICT => Ok(false),
            _ => Err(unavailable()),
        }
    }

    pub async fn ensure(&self, app: &App, workspace: Uuid, owner: Uuid) -> Result<Uuid> {
        let machine = self.machine.as_ref().ok_or_else(missing)?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO managed_computers(workspace,owner_id,id) VALUES($1,$2,$3)
            ON CONFLICT(workspace,owner_id) DO UPDATE SET touched_at=now() RETURNING id",
        )
        .bind(workspace)
        .bind(owner)
        .bind(Uuid::new_v4())
        .fetch_one(&app.db)
        .await?;
        let mut tx = app.db.begin().await?;
        sqlx::query("SELECT id FROM managed_computers WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let name = name(id);
        if let Some(pod) = self.pod(&name).await? {
            if pod["metadata"]["deletionTimestamp"].is_null()
                && matches!(
                    pod["status"]["phase"].as_str(),
                    Some("Failed" | "Succeeded")
                )
                && let Some(uid) = pod["metadata"]["uid"].as_str()
            {
                self.delete(&format!("pods/{name}"), Some(uid)).await?;
            }
        } else {
            self.create("persistentvolumeclaims", json!({
                "apiVersion":"v1","kind":"PersistentVolumeClaim",
                "metadata":{"name":name,"labels":{"app.kubernetes.io/name":"venticode-computer"}},
                "spec":{"accessModes":["ReadWriteOncePod"],"storageClassName":machine.storage_class,
                    "resources":{"requests":{"storage":"10Gi"}}}
            })).await?;
            self.create("pods", self.manifest(id, machine)).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    fn manifest(&self, id: Uuid, machine: &MachineConfig) -> Value {
        json!({
            "apiVersion":"v1","kind":"Pod",
            "metadata":{"name":name(id),"labels":{"app.kubernetes.io/name":"venticode-computer"}},
            "spec":{
                "hostUsers":false,"runtimeClassName":machine.runtime_class,"serviceAccountName":"computer",
                "automountServiceAccountToken":false,"enableServiceLinks":false,
                "restartPolicy":"Never","terminationGracePeriodSeconds":20,
                "containers":[{
                    "name":"computer","image":machine.image,
                    "env":[
                        {"name":"VENTICODE_MANAGED_ORIGIN","value":machine.origin},
                        {"name":"VENTICODE_MANAGED_ID","value":id},
                        {"name":"VENTICODE_BROWSER_ORIGINS","value":serde_json::to_string(&self.origins).expect("origin list")}
                    ],
                    "securityContext":{"runAsUser":0,"allowPrivilegeEscalation":false,
                        "seccompProfile":{"type":"Localhost","localhostProfile":machine.seccomp},
                        "capabilities":{"drop":["ALL"],"add":["CHOWN","DAC_OVERRIDE","FOWNER","KILL","SETUID","SETGID","SETPCAP"]}},
                    "resources":{"requests":{"cpu":"250m","memory":"768Mi","ephemeral-storage":"256Mi"},
                        "limits":{"cpu":"2","memory":"3Gi","ephemeral-storage":"2Gi"}},
                    "volumeMounts":[
                        {"name":"data","mountPath":"/data"},
                        {"name":"run","mountPath":"/run"},
                        {"name":"tmp","mountPath":"/tmp"},
                        {"name":"shm","mountPath":"/dev/shm"},
                        {"name":"identity","mountPath":"/var/run/secrets/venticode-computer","readOnly":true}
                    ]
                }],
                "volumes":[
                    {"name":"data","persistentVolumeClaim":{"claimName":name(id)}},
                    {"name":"run","emptyDir":{"medium":"Memory","sizeLimit":"64Mi"}},
                    {"name":"tmp","emptyDir":{"sizeLimit":"512Mi"}},
                    {"name":"shm","emptyDir":{"medium":"Memory","sizeLimit":"512Mi"}},
                    {"name":"identity","projected":{"defaultMode":256,"sources":[
                        {"serviceAccountToken":{"audience":AUDIENCE,"expirationSeconds":600,"path":"token"}}
                    ]}}
                ]
            }
        })
    }

    async fn touch(&self, app: &App, id: Uuid) -> Result<()> {
        if sqlx::query("UPDATE managed_computers SET touched_at=now() WHERE id=$1 AND NOT EXISTS(
                SELECT FROM erased_subjects WHERE workspace=managed_computers.workspace AND owner_id=managed_computers.owner_id)")
            .bind(id).execute(&app.db).await?.rows_affected() == 1 {
            return Ok(());
        }
        Err(missing())
    }

    pub async fn ready(
        &self,
        app: &App,
        workspace: Uuid,
        owner: Uuid,
    ) -> Result<Arc<runner::Runner>> {
        let id = self.ensure(app, workspace, owner).await?;
        for step in 0..75 {
            if let Some(runner) = self.runner(id).filter(|runner| runner.online()) {
                return Ok(runner);
            }
            if step > 0 && step % 8 == 0 {
                self.ensure(app, workspace, owner).await?;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Il computer non è pronto / Computer did not become ready".into(),
        ))
    }

    pub async fn erase(&self, app: &App, workspace: Uuid, owner: Uuid) -> Result<bool> {
        let Some(id) = self.id(app, workspace, owner).await? else {
            return Ok(true);
        };
        let name = name(id);
        if let Some(pod) = self.pod(&name).await? {
            let uid = pod["metadata"]["uid"].as_str().ok_or_else(unavailable)?;
            self.delete(&format!("pods/{name}"), Some(uid)).await?;
            return Ok(false);
        }
        if !self
            .delete(&format!("persistentvolumeclaims/{name}"), None)
            .await?
        {
            return Ok(false);
        }
        let mut tx = app.db.begin().await?;
        sqlx::query("DELETE FROM standalone_runner_inventory WHERE workspace=$1 AND runner_id=$2")
            .bind(workspace)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM managed_computers WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.runners.lock().unwrap().remove(&id);
        self.displays.lock().unwrap().remove(&id);
        Ok(true)
    }
}

fn name(id: Uuid) -> String {
    format!("computer-{}", id.simple())
}

pub(super) async fn enroll(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response> {
    let computers = app.computers.as_ref().ok_or_else(missing)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| s.len() <= 8192)
        .ok_or_else(forbidden)?;
    let workspace: Uuid = sqlx::query_scalar(
        "SELECT workspace FROM managed_computers m WHERE id=$1 AND NOT EXISTS (
        SELECT FROM erased_subjects WHERE workspace=m.workspace AND owner_id=m.owner_id)",
    )
    .bind(id)
    .fetch_one(&app.db)
    .await?;
    let review: Value = computers.request(Method::POST, "/apis/authentication.k8s.io/v1/tokenreviews").await?
        .json(&json!({"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","spec":{"token":token,"audiences":[AUDIENCE]}}))
        .send().await.map_err(|_| unavailable())?.error_for_status().map_err(|_| unavailable())?.json().await.map_err(|_| unavailable())?;
    let pod = computers.pod(&name(id)).await?.ok_or_else(forbidden)?;
    let uid = pod["metadata"]["uid"].as_str().ok_or_else(forbidden)?;
    let identity = &review["status"];
    if identity["authenticated"] != true
        || identity["audiences"] != json!([AUDIENCE])
        || identity["user"]["username"]
            != format!("system:serviceaccount:{}:computer", computers.namespace)
        || identity["user"]["extra"]["authentication.kubernetes.io/pod-uid"] != json!([uid])
        || !pod["metadata"]["deletionTimestamp"].is_null()
    {
        return Err(forbidden());
    }
    let runner = computers
        .runners
        .lock()
        .unwrap()
        .entry(id)
        .or_insert_with(|| Arc::new(runner::Runner::new(id, "")))
        .clone();
    Ok(upgrade
        .max_message_size(8 * 1024 * 1024)
        .max_frame_size(8 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            runner::serve(&runner, &app.db, workspace, socket).await
        }))
}

pub(super) async fn maintain(app: App) {
    let Some(computers) = &app.computers else {
        return;
    };
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if let Err(error) = reap(&app, computers).await {
            eprintln!("Managed workload reconciliation failed: {}", error.0);
        }
    }
}

async fn reap(app: &App, computers: &Computers) -> Result<()> {
    let response: Value = computers
        .request(Method::GET, &computers.namespaced("pods"))
        .await?
        .query(&[(
            "labelSelector",
            "app.kubernetes.io/name in (venticode-computer,venticode-kitesurf)",
        )])
        .send()
        .await
        .map_err(|_| unavailable())?
        .error_for_status()
        .map_err(|_| unavailable())?
        .json()
        .await
        .map_err(|_| unavailable())?;
    let mut machines = HashMap::new();
    let now = chrono::Utc::now();
    for pod in response["items"].as_array().ok_or_else(unavailable)? {
        let (Some(name), Some(uid)) = (
            pod["metadata"]["name"].as_str(),
            pod["metadata"]["uid"].as_str(),
        ) else {
            continue;
        };
        if let Some(id) = name
            .strip_prefix("computer-")
            .and_then(|raw| raw.parse::<Uuid>().ok())
        {
            machines.insert(id, uid);
        } else if name.starts_with("kitesurf-")
            && (matches!(
                pod["status"]["phase"].as_str(),
                Some("Succeeded" | "Failed")
            ) || pod["metadata"]["creationTimestamp"]
                .as_str()
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
                .is_some_and(|start| (now - start).num_seconds() > 990))
        {
            computers.delete(&format!("pods/{name}"), Some(uid)).await?;
        }
    }
    let live: Vec<_> = machines.keys().copied().collect();
    let mut tx = app.db.begin().await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT m.id FROM managed_computers m
        WHERE m.id=ANY($1) AND m.touched_at < now()-interval '10 minutes' AND NOT EXISTS (
            SELECT FROM conversations c JOIN projects p ON p.id=c.project_id
            WHERE p.harness_id=m.id AND c.active_run IS NOT NULL AND c.lease_until > now())
        ORDER BY m.touched_at LIMIT 8 FOR UPDATE OF m SKIP LOCKED",
    )
    .bind(live)
    .fetch_all(&mut *tx)
    .await?;
    for id in ids {
        computers
            .delete(&format!("pods/{}", name(id)), Some(machines[&id]))
            .await?;
        computers.runners.lock().unwrap().remove(&id);
        computers.displays.lock().unwrap().remove(&id);
    }
    tx.commit().await?;
    Ok(())
}

async fn status(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    let computers = app
        .computers
        .as_ref()
        .filter(|c| c.machine.is_some())
        .ok_or_else(missing)?;
    let member = Member::load(&app, &headers, false).await?;
    if let Some(id) = computers.id(&app, member.workspace, member.user).await? {
        let state = if computers.runner(id).is_some_and(|r| r.online()) {
            "ready"
        } else if computers.pod(&name(id)).await?.is_some_and(|pod| {
            pod["metadata"]["deletionTimestamp"].is_null()
                && !matches!(
                    pod["status"]["phase"].as_str(),
                    Some("Failed" | "Succeeded")
                )
        }) {
            "starting"
        } else {
            "stopped"
        };
        return Ok(Json(json!({"state":state,"harness_id":id})));
    }
    Ok(Json(json!({"state":"stopped"})))
}

pub(super) fn is_operation(op: &str) -> bool {
    browser::is_browser_tool(op)
        || matches!(
            op,
            "desktop_frame"
                | "desktop_input"
                | "desktop_release"
                | "browser_open_preview"
                | "ask_model"
                | "project_service_start"
                | "project_service_stop"
        )
}

pub(super) async fn rpc(runner: &runner::Runner, op: &str, args: Value) -> Result<Value> {
    let id = Uuid::new_v4();
    let session = id.to_string();
    runner.action(session.clone(), &json!({"action":"execute","request_id":id,"project_id":id,
        "harness_id":runner.id(),"root":"work","project_path":".","operation":op,"arguments":args,"managed":true}))?;
    let result = tokio::time::timeout(Duration::from_secs(12), async {
        let mut after = 0;
        let mut result = None;
        loop {
            let poll = runner.action(
                session.clone(),
                &json!({"action":"poll","request_id":id,"after":after}),
            )?;
            for frame in poll["frames"].as_array().ok_or_else(unavailable)? {
                if frame["result"]["type"] == "result" {
                    result = Some(frame["result"]["value"].clone());
                }
                if frame["result"]["type"] == "error" {
                    return Err(Error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        frame["result"]["error"]
                            .as_str()
                            .unwrap_or("Computer operation failed")
                            .to_owned(),
                    ));
                }
            }
            after = poll["after"].as_i64().ok_or_else(unavailable)?;
            match poll["state"].as_str() {
                Some("done") => return result.ok_or_else(unavailable),
                Some("failed" | "cancelled") => return Err(unavailable()),
                _ => tokio::time::sleep(Duration::from_millis(35)).await,
            }
        }
    })
    .await;
    if result.is_err() || result.as_ref().is_ok_and(|value| value.is_err()) {
        let _ = runner.action(session, &json!({"action":"cancel","request_id":id}));
    }
    runner.forget(id);
    result.map_err(|_| unavailable())?
}

async fn screen(
    State(app): State<App>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response> {
    let computers = app
        .computers
        .clone()
        .filter(|c| c.machine.is_some())
        .ok_or_else(missing)?;
    let member = Member::load(&app, &headers, true).await?;
    Ok(upgrade
        .max_message_size(24 * 1024)
        .on_upgrade(move |mut ws| async move {
            if let Err(error) = view(&app, &computers, &member, &headers, &mut ws).await {
                let _ = send(
                    &mut ws,
                    Message::Text(
                        json!({"type":"ended","reason":if error.0 == StatusCode::CONFLICT {
                "busy"
            } else { "unavailable" }})
                        .to_string()
                        .into(),
                    ),
                )
                .await;
            }
        }))
}

async fn send(ws: &mut WebSocket, message: Message) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), ws.send(message))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
}

async fn view(
    app: &App,
    computers: &Computers,
    member: &Member,
    headers: &HeaderMap,
    ws: &mut WebSocket,
) -> Result<()> {
    let runner = computers.ready(app, member.workspace, member.user).await?;
    let id = runner.id();
    let display = computers.display(id);
    let mut human = None;
    let mut frames = tokio::time::interval(Duration::from_millis(280));
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut lease = tokio::time::interval(Duration::from_secs(15));
    send(
        ws,
        Message::Text(json!({"type":"ready"}).to_string().into()),
    )
    .await?;
    let result = async {
        loop {
            tokio::select! {
                packet = ws.recv() => {
                    let Some(Ok(packet)) = packet else { break };
                    let Message::Text(text) = packet else { continue };
                    let value: Value = serde_json::from_str(&text).map_err(|_| bad())?;
                    match value["type"].as_str() {
                        Some("end") => break,
                        Some("take") if human.is_none() => {
                            human = display.clone().try_write_owned().ok();
                            send(ws, Message::Text(json!({"type":"control","human":human.is_some()}).to_string().into())).await?;
                        }
                        Some("release") if human.is_some() => {
                            rpc(&runner, "desktop_release", json!({})).await?;
                            human = None;
                            send(ws, Message::Text(json!({"type":"control","human":false}).to_string().into())).await?;
                        }
                        Some("input") if human.is_some() && value["tab"] == DESKTOP => {
                            rpc(&runner, "desktop_input", value["input"].clone()).await?;
                        }
                        _ => {}
                    }
                }
                _ = frames.tick() => {
                    let frame = rpc(&runner, "desktop_frame", json!({})).await?;
                    let bytes = STANDARD.decode(frame["jpeg"].as_str().ok_or_else(unavailable)?).map_err(|_| unavailable())?;
                    let mut packet = json!({"type":"frame","tab":DESKTOP,"width":frame["width"],"height":frame["height"]})
                        .to_string().into_bytes();
                    packet.push(b'\n');
                    packet.extend(bytes);
                    send(ws, Message::Binary(packet.into())).await?;
                }
                _ = lease.tick() => {
                    let current = Member::load(app,headers,true).await?;
                    if current.workspace != member.workspace || current.user != member.user { return Err(forbidden()); }
                    computers.touch(app, id).await?;
                }
            }
        }
        Ok(())
    }.await;
    if human.is_some() {
        let _ = rpc(&runner, "desktop_release", json!({})).await;
    }
    result
}

pub(super) struct PreviewCleanup {
    pub runner: Arc<runner::Runner>,
    pub run: Uuid,
}

impl Drop for PreviewCleanup {
    fn drop(&mut self) {
        let runner = self.runner.clone();
        let run = self.run;
        tokio::spawn(async move {
            let _ = rpc(&runner, "project_service_stop", json!({"run_id":run})).await;
        });
    }
}

pub(super) async fn complete_model_request(
    app: &App,
    h: &HeaderMap,
    conversation: Uuid,
    run: Uuid,
    call: Uuid,
    runner: &runner::Runner,
    payload: &Value,
) -> Result<()> {
    let query: Uuid = serde_json::from_value(payload["query"].clone()).map_err(|_| bad())?;
    let result = subcompletion(app, h, conversation, run, query, payload).await;
    let mut response = match result {
        Ok(result) => result,
        Err(error) => json!({"error":error.1}),
    };
    response["id"] = json!(call);
    response["query"] = json!(query);
    response["type"] = json!("model_result");
    runner.respond(call, response)
}

async fn subcompletion(
    app: &App,
    h: &HeaderMap,
    conversation: Uuid,
    run: Uuid,
    query: Uuid,
    payload: &Value,
) -> Result<Value> {
    use futures_util::StreamExt;
    let prompt = payload["prompt"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 32768)
        .ok_or_else(bad)?;
    let member = Member::load(app, h, true).await?;
    let row = projects::owned(app, &member, conversation).await?;
    if row.get::<&str, _>("runtime") != "computer" {
        return Err(forbidden());
    }
    let mut tx = app.db.begin().await?;
    let model: String = sqlx::query_scalar(
        "SELECT model FROM conversations WHERE id=$1 AND active_run=$2 FOR UPDATE",
    )
    .bind(conversation)
    .bind(run)
    .fetch_one(&mut *tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO computer_model_calls(run_id,id)
        SELECT $1,$2 WHERE (SELECT count(*) FROM computer_model_calls WHERE run_id=$1)<4
        AND NOT EXISTS(SELECT FROM computer_model_calls WHERE run_id=$1 AND NOT finished)
        ON CONFLICT DO NOTHING",
    )
    .bind(run)
    .bind(query)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted != 1 {
        return Err(Error(
            StatusCode::TOO_MANY_REQUESTS,
            "Limite richieste al modello / Model request limit reached".into(),
        ));
    }
    tx.commit().await?;
    let request = json!({"model":model,"messages":[{"role":"user","content":prompt}],
        "stream":true,"stream_options":{"include_usage":true},"max_tokens":2048});
    let result = tokio::time::timeout(Duration::from_secs(19), async {
        let mut events = inference::infer(app, h, &request).await?;
        let mut result = String::new();
        let mut usage = Value::Null;
        while let Some(event) = events.next().await {
            let event = event?;
            if event == "[DONE]" {
                return Ok(json!({"model":model,"text":result,"usage":usage}));
            }
            let value: Value = serde_json::from_str(&event).map_err(|_| unavailable())?;
            if value.get("error").is_some() {
                return Err(unavailable());
            }
            if let Some(text) = value["choices"][0]["delta"]["content"].as_str() {
                if result.len() + text.len() > 128 * 1024 {
                    return Err(unavailable());
                }
                result.push_str(text);
            }
            if !value["usage"].is_null() {
                usage = value["usage"].clone();
            }
        }
        Err(unavailable())
    })
    .await
    .unwrap_or_else(|_| Err(unavailable()));
    sqlx::query("UPDATE computer_model_calls SET finished=true WHERE id=$1")
        .bind(query)
        .execute(&app.db)
        .await?;
    let mut event = json!({"type":"run","run_id":run,"state":"running",
        "model_call":query,"model":model,"status":if result.is_ok() {"ok"} else {"error"}});
    if let Ok(value) = &result {
        let usage = &value["usage"];
        let input = usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64());
        let output = usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64());
        let tokens: serde_json::Map<String, Value> =
            [("input_tokens", input), ("output_tokens", output)]
                .into_iter()
                .filter_map(|(key, value)| value.map(|value| (key.into(), json!(value))))
                .collect();
        if !tokens.is_empty() {
            event["model_usage"] = json!(tokens);
        }
    }
    runs::record(app, conversation, run, event).await?;
    result
}
