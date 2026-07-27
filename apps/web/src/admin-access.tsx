import { useId, useState, type FormEvent } from "react";
import type {
  AdminAccessKeyMethod,
  Session,
} from "@opensoverignblog/sdk";
import { asMessage, client, text } from "./lib";

export interface AdminAccessKeyFormProps {
  autoFocus?: boolean;
  method: AdminAccessKeyMethod;
  onAuthenticated: (session: Session) => void;
  showDivider?: boolean;
  submitLabel?: string;
}

/**
 * Exchanges the administrator key directly with this OSB instance.
 *
 * The credential intentionally lives only in component memory for the lifetime
 * of this form submission. The SDK constrains the advertised action to the
 * same-origin auth namespace and exchanges the key for an HttpOnly session.
 */
export function AdminAccessKeyForm({
  autoFocus = false,
  method,
  onAuthenticated,
  showDivider = false,
  submitLabel,
}: AdminAccessKeyFormProps) {
  const inputId = useId();
  const introId = `${inputId}-intro`;
  const hintId = `${inputId}-hint`;
  const [accessKey, setAccessKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<{ kind: "status" | "error"; text: string }>();

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!accessKey || busy) return;
    setBusy(true);
    setNotice({ kind: "status", text: text("관리자 권한을 확인하는 중…", "Verifying administrator access…") });
    try {
      const next = await client.loginWithAdminAccessKey(
        { accessKey },
        method.actionHref,
      );
      onAuthenticated(next);
    } catch (reason) {
      setNotice({ kind: "error", text: asMessage(reason) });
    } finally {
      setAccessKey("");
      setBusy(false);
    }
  }

  return (
    <form className="auth-form admin-access-form" onSubmit={(event) => void submit(event)}>
      {showDivider ? <div className="auth-divider"><span>{text("또는", "or")}</span></div> : null}
      <p className="admin-access-intro" id={introId}>
        <strong>{text("관리자 Access Key 입력", "Enter administrator access key")}</strong>
        <span>
          {text(
            "이 OSB 서버를 설치할 때 설정한 관리자 Access Key를 아래 칸에 입력하세요.",
            "Enter the administrator access key configured when this OSB server was installed.",
          )}
        </span>
      </p>
      <label htmlFor={inputId}>
        <span>{text("관리자 Access Key", "Administrator access key")}</span>
        <input
          aria-describedby={`${introId} ${hintId}`}
          autoCapitalize="none"
          autoComplete="off"
          autoFocus={autoFocus}
          id={inputId}
          maxLength={512}
          minLength={32}
          onChange={(event) => setAccessKey(event.target.value)}
          placeholder={text(
            "설치 시 설정한 관리자 Access Key를 붙여넣으세요",
            "Paste the administrator access key configured during setup",
          )}
          required
          spellCheck={false}
          type="password"
          value={accessKey}
        />
      </label>
      <p className="field-hint" id={hintId}>
        {text(
          "접근 키는 이 서버에 관리자 세션을 만드는 요청에만 사용되며 브라우저 저장소에 보관하지 않습니다.",
          "The access key is used only to create an administrator session on this server and is never stored in browser storage.",
        )}
      </p>
      <button className="button button-primary button-wide" disabled={busy || !accessKey} type="submit">
        {busy
          ? text("관리자 키를 확인하는 중…", "Verifying administrator key…")
          : submitLabel ?? text("관리자 키로 계속", "Continue with administrator key")}
      </button>
      {notice ? (
        <p className="inline-status" role={notice.kind === "error" ? "alert" : "status"}>
          {notice.text}
        </p>
      ) : null}
    </form>
  );
}
