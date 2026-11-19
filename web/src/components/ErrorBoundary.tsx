import { Component, type ReactNode } from "react";
import { t } from "../i18n/index.ts";

interface Props {
  children: ReactNode;
  resetKey?: string;
  inline?: boolean;
  reloadOnRetry?: boolean;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidUpdate(prevProps: Props) {
    if (this.state.error && prevProps.resetKey !== this.props.resetKey) {
      this.setState({ error: null });
    }
  }

  retry = () => {
    if (this.props.reloadOnRetry) {
      location.reload();
      return;
    }
    this.setState({ error: null });
  };

  render() {
    if (this.state.error) {
      if (this.props.inline) {
        return (
          <div className="widget-error" role="alert">
            <p className="error-line" style={{ margin: 0 }}>{t("Si è verificato un errore imprevisto")}</p>
            {this.state.error.message && <p className="muted" style={{ margin: 0, fontSize: 12 }}>{this.state.error.message}</p>}
            <button type="button" className="btn" onClick={this.retry}>{t("Riprova")}</button>
          </div>
        );
      }
      return (
        <main className="empty" role="alert">
          <h1>{t("Si è verificato un errore imprevisto")}</h1>
          <p>{this.state.error.message}</p>
          <button type="button" className="btn btn-primary" onClick={this.retry}>{t("Riprova")}</button>
        </main>
      );
    }
    return this.props.children;
  }
}
