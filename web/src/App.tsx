import { useState } from "react";
import { Navigate, Route, Routes, useLocation } from "react-router-dom";
import { LoaderCircle, Menu } from "lucide-react";
import { isUnauthorized, useSession } from "./queries.ts";
import { t, useLanguage } from "./i18n/index.ts";
import { Sidebar } from "./components/Sidebar.tsx";
import { Home } from "./components/Home.tsx";
import { ProjectView } from "./components/ProjectView.tsx";
import { ConversationView } from "./components/ConversationView.tsx";
import { BotView } from "./components/BotView.tsx";

import { Connections } from "./components/Connections.tsx";

export function App() {
  useLanguage();
  const session = useSession();
  const [navOpen, setNavOpen] = useState(false);
  const location = useLocation();
  const withPanel = location.pathname.startsWith("/c/");

  if (session.isPending)
    return <div className="empty" role="status"><LoaderCircle className="spin" aria-hidden="true" />{t("Caricamento…")}</div>;
  if (session.isError) return isUnauthorized(session.error) ? <SignedOut /> : <Failure message={session.error.message} />;

  const admin = ["tenant_admin", "reseller_admin"].includes(session.data.role);
  const menu = (
    <button className="icon-btn menu-btn" aria-label={t("Apri il menu")} onClick={() => setNavOpen(true)}>
      <Menu size={18} aria-hidden="true" />
    </button>
  );
  return (
    <div className={`shell${withPanel ? " with-panel" : ""}${navOpen ? " nav-open" : ""}`}>
      <Sidebar session={session.data} onNavigate={() => setNavOpen(false)} />
      <button className="scrim" aria-label={t("Chiudi il menu")} onClick={() => setNavOpen(false)} />
      <Routes>
        <Route path="/" element={<Home session={session.data} menu={menu} />} />
        <Route path="/p/:id" element={<ProjectView menu={menu} />} />
        <Route path="/c/:id" element={<ConversationView menu={menu} />} />
        <Route path="/bots/:id" element={<BotView menu={menu} admin={admin} />} />
        <Route path="/connections" element={<Connections menu={menu} admin={admin} />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </div>
  );
}

function SignedOut() {
  return (
    <main className="empty">
      <div className="brand-mark" aria-hidden="true">&gt;_</div>
      <h1>{t("Apri Venticode dal portale")}</h1>
      <p>{t("Venticode usa l'accesso del portale Ventic. Apri Venticode dal menu del portale per entrare in questo workspace.")}</p>
      {import.meta.env.DEV && <a className="btn btn-primary btn-lg" href="/__dev/login">{t("Accesso di sviluppo (dati sintetici)")}</a>}
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
