import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Navigate, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { LoaderCircle, Menu, PanelRight } from "lucide-react";
import { isUnauthorized, useSession, useProjects, useConversations } from "./queries.ts";
import { t, useLanguage } from "./i18n/index.ts";
import { ErrorBoundary } from "./components/ErrorBoundary.tsx";
import { Sidebar } from "./components/Sidebar.tsx";
import { Home } from "./components/Home.tsx";
import { ProjectView } from "./components/ProjectView.tsx";
import { ConversationView } from "./components/ConversationView.tsx";

const BotView = lazy(() => import("./components/BotView.tsx").then((m) => ({ default: m.BotView })));
const DesktopView = lazy(() => import("./components/DesktopView.tsx").then((m) => ({ default: m.DesktopView })));
const BrowserView = lazy(() => import("./components/BrowserView.tsx").then((m) => ({ default: m.BrowserView })));
const Connections = lazy(() => import("./components/Connections.tsx").then((m) => ({ default: m.Connections })));

export function App() {
  return <ErrorBoundary reloadOnRetry><AppShell /></ErrorBoundary>;
}

function AppShell() {
  useLanguage();
  const session = useSession();
  const [navOpen, setNavOpen] = useState(false);
  const [panelOpen, setPanelOpen] = useState(false);
  const location = useLocation();
  const withPanel = location.pathname.startsWith("/c/");

  useEffect(() => {
    setNavOpen(false);
    setPanelOpen(false);
  }, [location.pathname]);

  useEffect(() => {
    if (!navOpen && !panelOpen) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !document.querySelector("dialog[open]")) {
        event.preventDefault();
        event.stopPropagation();
        event.stopImmediatePropagation();
        setNavOpen(false);
        setPanelOpen(false);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [navOpen, panelOpen]);

  if (session.isPending)
    return <div className="empty" role="status"><LoaderCircle className="spin" aria-hidden="true" />{t("Caricamento…")}</div>;
  if (session.isError) return isUnauthorized(session.error) ? <SignedOut /> : <Failure message={session.error.message} />;

  const admin = ["tenant_admin", "reseller_admin"].includes(session.data.role);
  const menu = (
    <button className="icon-btn menu-btn" aria-label={t("Apri il menu")} aria-expanded={navOpen} onClick={() => setNavOpen(true)}>
      <Menu size={18} aria-hidden="true" />
    </button>
  );
  const panelToggle = (
    <button className="icon-btn panel-toggle" aria-label={t("Dettagli sessione")} title={t("Dettagli sessione")} aria-expanded={panelOpen} onClick={() => setPanelOpen((v) => !v)}>
      <PanelRight size={17} aria-hidden="true" />
    </button>
  );
  const routeFallback = (
    <main className="main">
      <div className="empty" role="status">
        <LoaderCircle className="spin" aria-hidden="true" />
        {t("Caricamento…")}
      </div>
    </main>
  );
  return (
    <div className={`shell${withPanel ? " with-panel" : ""}${navOpen ? " nav-open" : ""}${panelOpen ? " panel-open" : ""}`}>
      <WorkspaceShortcuts />
      <Sidebar session={session.data} onNavigate={() => setNavOpen(false)} />
      <button className="scrim" aria-label={t("Chiudi il menu")} onClick={() => { setNavOpen(false); setPanelOpen(false); }} />
      <ErrorBoundary resetKey={location.pathname}>
        <Suspense fallback={routeFallback}>
          <Routes>
            <Route path="/" element={<Home session={session.data} menu={menu} />} />
            <Route path="/p/:id" element={<ProjectView menu={menu} />} />
            <Route path="/c/:id" element={<ConversationView menu={menu} panelToggle={panelToggle} />} />
            <Route path="/bots/:id" element={<BotView menu={menu} admin={admin} />} />
            <Route path="/connections" element={<Connections menu={menu} admin={admin} />} />
            <Route path="/browser" element={<BrowserView menu={menu} />} />
            {session.data.bot_runtimes?.includes("computer") && <Route path="/computer" element={<DesktopView menu={menu} />} />}
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </Suspense>
      </ErrorBoundary>
    </div>
  );
}

function SignedOut() {
  const browser = useLocation().pathname === "/browser";
  return (
    <main className="empty">
      <div className="brand-mark" aria-hidden="true">&gt;_</div>
      <h1>{t("Accedi a Venticode")}</h1>
      <a className="btn btn-primary btn-lg" href={browser ? "/venticode/login?browser=1" : "/venticode/login"}>{t("Accedi")}</a>
      {import.meta.env.DEV && <a className="btn btn-primary btn-lg" href={browser ? "/__dev/login?browser=1" : "/__dev/login"}>{t("Accesso di sviluppo (dati sintetici)")}</a>}
    </main>
  );
}

function Failure({ message }: { message: string }) {
  return (
    <main className="empty" role="alert">
      <h1>{t("Venticode non è raggiungibile")}</h1>
      <p>{message}</p>
      <button className="btn" onClick={() => location.reload()}>{t("Riprova")}</button>
    </main>
  );
}

function WorkspaceShortcuts() {
  const [menu, setMenu] = useState<"projects"|"chats"|null>(null);
  const dialog = useRef<HTMLDialogElement>(null);
  const navigate = useNavigate();
  const location = useLocation();
  const projects = useProjects().data || [];
  const chats = useConversations().data || [];
  const id = location.pathname.split("/")[2];
  const project = location.pathname.startsWith("/p/") ? id : chats.find(c=>c.id===id)?.project_id;
  useEffect(()=>{
    const listen = (event: KeyboardEvent) => {
      const active = document.activeElement as HTMLElement | null;
      const inField = active && (["INPUT", "TEXTAREA", "SELECT"].includes(active.tagName) || active.isContentEditable || active.hasAttribute("data-remote-desktop") || active.getAttribute("data-controlled") === "true");
      if ((event.key !== "F2" && event.key !== "F3") || document.querySelector("dialog[open]") || inField) return;
      event.preventDefault();
      setMenu(event.key === "F2" ? "chats" : "projects");
    };
    window.addEventListener("keydown",listen);
    return ()=>window.removeEventListener("keydown",listen);
  },[]);
  useEffect(()=>{if(menu)dialog.current?.showModal();},[menu]);
  if (!menu) return null;
  const items = menu==="projects" ? projects.map(p=>({id:p.id,label:p.name,href:"/p/"+p.id})):
    [...(project?[{id:"new",label:t("Nuova conversazione"),href:"/p/"+project}]:[]),
      ...chats.filter(c=>!project || c.project_id===project).map(c=>({id:c.id,label:c.title,href:"/c/"+c.id}))];
  const openChoice = (href: string | null | undefined) => {
    if (!href) return;
    dialog.current?.close();
    navigate(href);
  };
  return <dialog ref={dialog} onClose={()=>setMenu(null)} onClick={e=>{if(e.target===dialog.current)dialog.current.close();}} aria-labelledby="picker-title">
    <form onSubmit={e=>{e.preventDefault();const choice=new FormData(e.currentTarget).get("choice");if(typeof choice==="string")openChoice(choice);}}>
      <h2 id="picker-title">{menu==="projects"?t("Scegli un progetto"):t("Conversazioni")}</h2>
      <select autoFocus className="select" name="choice" aria-labelledby="picker-title" size={Math.max(2,Math.min(8,items.length))} defaultValue={items[0]?.href}
        onDoubleClick={e=>openChoice(e.currentTarget.value)}
        onKeyDown={e=>{if(e.key==="Enter"){e.preventDefault();openChoice(e.currentTarget.value);}}}>
        {items.map(item=><option key={item.id} value={item.href}>{item.label}</option>)}
      </select>
      <div className="dialog-actions"><button type="button" className="btn" onClick={()=>dialog.current?.close()}>{t("Annulla")}</button>
        <button className="btn btn-primary" disabled={!items.length}>{t("Apri")}</button></div>
    </form>
  </dialog>;
}
