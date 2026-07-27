import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import type {
  AdvertisingCapabilities,
  AdvertisingConsentDecision,
  AdvertisingPlacement,
  AdvertisingViewport,
} from "@opensoverignblog/sdk";
import {
  advertisingUnitFor,
  installKakaoAdFitLoader,
  isConfirmedAdvertisingReaderContent,
  isSupportedAdvertising,
} from "./advertising-policy";
import { asMessage, client, text } from "./lib";

interface AdvertisingContextValue {
  active: boolean;
  advertising: AdvertisingCapabilities | undefined;
  authorized: boolean;
  checked: boolean;
  decision: AdvertisingConsentDecision;
  openSettings: () => void;
  pathname: string;
  viewport: AdvertisingViewport;
}

interface ConsentState {
  checked: boolean;
  decision: AdvertisingConsentDecision;
  key: string;
}

const AdvertisingContext = createContext<AdvertisingContextValue | undefined>(undefined);

export function KakaoAdFitProvider({
  advertising,
  children,
  contentReady,
  pathname,
}: {
  advertising: AdvertisingCapabilities | undefined;
  children: ReactNode;
  contentReady: boolean;
  pathname: string;
}) {
  const supportedAdvertising = isSupportedAdvertising(advertising) ? advertising : undefined;
  const active = Boolean(
    supportedAdvertising
    && isConfirmedAdvertisingReaderContent(pathname, contentReady),
  );
  const viewport = useAdvertisingViewport();
  const configurationKey = supportedAdvertising
    ? advertisingConfigurationKey(supportedAdvertising)
    : "";
  const [consent, setConsent] = useState<ConsentState>({
    checked: false,
    decision: "unknown",
    key: "",
  });
  const [panelOpen, setPanelOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  useEffect(() => {
    if (!active || !supportedAdvertising) {
      setPanelOpen(false);
      setError(undefined);
      return;
    }
    const controller = new AbortController();
    setConsent((current) => current.key === configurationKey
      ? current
      : { checked: false, decision: "unknown", key: configurationKey });
    setError(undefined);
    void client
      .advertisingConsent(
        supportedAdvertising.consent.statusHref,
        controller.signal,
      )
      .then((status) => {
        if (controller.signal.aborted) return;
        if (!isConsentDecision(status.decision)) {
          throw new TypeError("advertising consent response is invalid");
        }
        setConsent({
          checked: true,
          decision: status.decision,
          key: configurationKey,
        });
        setPanelOpen(status.decision === "unknown");
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        setConsent({
          checked: true,
          decision: "unknown",
          key: configurationKey,
        });
        setError(asMessage(reason));
        setPanelOpen(true);
      });
    return () => controller.abort();
  }, [active, configurationKey, supportedAdvertising]);

  const checked = active
    && consent.key === configurationKey
    && consent.checked;
  const decision = checked ? consent.decision : "unknown";
  const authorized = checked && decision === "granted";

  useEffect(() => {
    if (!authorized || !supportedAdvertising) return undefined;
    return installKakaoAdFitLoader(
      document,
      supportedAdvertising.scriptUrl,
    );
  }, [
    authorized,
    configurationKey,
    pathname,
    supportedAdvertising,
    viewport,
  ]);

  async function decide(nextDecision: "granted" | "denied") {
    if (!active || !supportedAdvertising || busy) return;
    setBusy(true);
    setError(undefined);
    if (nextDecision === "denied") {
      // Withdraw immediately, before the persistence request can complete.
      setConsent({
        checked: true,
        decision: "denied",
        key: configurationKey,
      });
    }
    try {
      const status = await client.setAdvertisingConsent(
        { decision: nextDecision },
        supportedAdvertising.consent.actionHref,
      );
      if (status.decision !== nextDecision) {
        throw new TypeError("advertising consent response did not confirm the decision");
      }
      setConsent({
        checked: true,
        decision: nextDecision,
        key: configurationKey,
      });
      setPanelOpen(false);
    } catch (reason) {
      setError(asMessage(reason));
      setPanelOpen(true);
    } finally {
      setBusy(false);
    }
  }

  const context = useMemo<AdvertisingContextValue>(
    () => ({
      active,
      advertising: supportedAdvertising,
      authorized,
      checked,
      decision,
      openSettings: () => setPanelOpen(true),
      pathname,
      viewport,
    }),
    [
      active,
      authorized,
      checked,
      decision,
      pathname,
      supportedAdvertising,
      viewport,
    ],
  );

  return (
    <AdvertisingContext.Provider value={context}>
      {children}
      {active && supportedAdvertising && panelOpen ? (
        <AdvertisingConsentDialog
          advertising={supportedAdvertising}
          busy={busy}
          decision={decision}
          error={error}
          onClose={() => setPanelOpen(false)}
          onDecide={(value) => void decide(value)}
        />
      ) : null}
    </AdvertisingContext.Provider>
  );
}

export function KakaoAdFitSlot({
  placement,
}: {
  placement: AdvertisingPlacement;
}) {
  const context = useAdvertising();
  if (
    !context?.active
    || !context.advertising
    || !context.authorized
  ) {
    return null;
  }
  const unit = advertisingUnitFor(
    context.advertising,
    placement,
    context.viewport,
  );
  if (!unit) return null;
  const style = {
    "--adfit-slot-height": `${unit.height}px`,
    "--adfit-slot-width": `${unit.width}px`,
  } as CSSProperties;
  const label = placement === "top"
    ? text("상단 광고", "Top advertisement")
    : text("하단 광고", "Bottom advertisement");

  return (
    <aside
      aria-label={label}
      className={`adfit-slot adfit-slot-${placement}`}
      data-adfit-decision={context.decision}
      style={style}
    >
      <span className="adfit-slot-label">{text("광고", "Advertisement")}</span>
      <div className="adfit-slot-reserved">
        <ins
          className="kakao_ad_area"
          data-ad-height={String(unit.height)}
          data-ad-unit={unit.unitId}
          data-ad-width={String(unit.width)}
          data-osb-adfit-placement={placement}
          key={`${context.pathname}:${placement}:${context.viewport}:${unit.unitId}`}
          style={{ display: "none", width: "100%" }}
        />
      </div>
    </aside>
  );
}

export function AdvertisingSettingsButton() {
  const context = useAdvertising();
  if (!context?.active) return null;
  return (
    <button
      aria-haspopup="dialog"
      className="footer-advertising-settings"
      onClick={context.openSettings}
      type="button"
    >
      {text("개인정보·광고 설정", "Privacy & ad settings")}
    </button>
  );
}

function AdvertisingConsentDialog({
  advertising,
  busy,
  decision,
  error,
  onClose,
  onDecide,
}: {
  advertising: AdvertisingCapabilities;
  busy: boolean;
  decision: AdvertisingConsentDecision;
  error: string | undefined;
  onClose: () => void;
  onDecide: (decision: "granted" | "denied") => void;
}) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const canClose = decision !== "unknown";
  useEffect(() => {
    if (dialogRef.current && !dialogRef.current.open) {
      dialogRef.current.showModal();
    }
    return () => dialogRef.current?.close();
  }, []);

  return (
    <dialog
      aria-describedby="adfit-consent-description"
      aria-labelledby="adfit-consent-title"
      className="adfit-consent-dialog"
      onCancel={(event) => {
        event.preventDefault();
        if (canClose) onClose();
      }}
      ref={dialogRef}
    >
      <div className="adfit-consent-heading">
        <div>
          <p className="eyebrow">Kakao AdFit</p>
          <h2 id="adfit-consent-title">
            {text("광고 및 개인정보 설정", "Advertising and privacy settings")}
          </h2>
        </div>
        {canClose ? (
          <button
            aria-label={text("광고 설정 닫기", "Close advertising settings")}
            className="dialog-close"
            onClick={onClose}
            type="button"
          >
            ×
          </button>
        ) : null}
      </div>
      <p id="adfit-consent-description">
        {text(
          "상단과 하단의 Kakao AdFit 광고를 표시하려면 광고 전송·측정·개인화 목적의 외부 연결에 동의해 주세요. 거부해도 모든 글을 그대로 읽을 수 있습니다.",
          "To show Kakao AdFit banners at the top and bottom, allow external connections for ad delivery, measurement, and personalization. You can still read every post if you decline.",
        )}
      </p>
      <details className="adfit-consent-details">
        <summary>{text("자세히", "Details")}</summary>
        <p>
          {text(
            "허용하기 전에는 광고 요소나 Kakao 광고 스크립트를 만들지 않습니다. 설정은 언제든 사이트 하단에서 바꿀 수 있습니다.",
            "No advertising element or Kakao advertising script is created before you allow it. You can change this choice from the site footer at any time.",
          )}
        </p>
        <p className="adfit-purpose-list">
          {text("사용 목적", "Purposes")}: {advertising.consent.purposeIds.join(", ")}
        </p>
        <div className="adfit-policy-links">
          <a href={advertising.consent.privacyHref} rel="noreferrer" target="_blank">
            {text("개인정보 처리 안내", "Privacy information")}
          </a>
          <a href={advertising.consent.policyHref} rel="noreferrer" target="_blank">
            {text("Kakao AdFit 운영정책", "Kakao AdFit policy")}
          </a>
        </div>
      </details>
      {error ? (
        <p className="adfit-consent-error" role="alert">
          {text("설정을 저장하지 못했습니다: ", "Could not save the setting: ")}
          {error}
        </p>
      ) : null}
      <div
        aria-label={text("광고 동의 선택", "Advertising consent choices")}
        className="adfit-consent-actions"
        role="group"
      >
        <button
          className="adfit-consent-choice"
          disabled={busy}
          onClick={() => onDecide("granted")}
          type="button"
        >
          {busy ? text("저장 중…", "Saving…") : text("모두 허용", "Allow all")}
        </button>
        <button
          className="adfit-consent-choice"
          disabled={busy}
          onClick={() => onDecide("denied")}
          type="button"
        >
          {busy ? text("저장 중…", "Saving…") : text("모두 거부", "Reject all")}
        </button>
      </div>
    </dialog>
  );
}

function useAdvertising(): AdvertisingContextValue | undefined {
  return useContext(AdvertisingContext);
}

function useAdvertisingViewport(): AdvertisingViewport {
  const [viewport, setViewport] = useState<AdvertisingViewport>(() => (
    typeof window.matchMedia === "function"
    && window.matchMedia("(max-width: 767px)").matches
      ? "mobile"
      : "pc"
  ));
  useEffect(() => {
    if (typeof window.matchMedia !== "function") return undefined;
    const media = window.matchMedia("(max-width: 767px)");
    const update = () => setViewport(media.matches ? "mobile" : "pc");
    update();
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  return viewport;
}

function advertisingConfigurationKey(
  advertising: AdvertisingCapabilities,
): string {
  return [
    advertising.provider,
    advertising.scriptUrl,
    advertising.policyVersion,
    advertising.placements.top.pc.unitId,
    advertising.placements.top.mobile.unitId,
    advertising.placements.bottom.pc.unitId,
    advertising.placements.bottom.mobile.unitId,
  ].join(":");
}

function isConsentDecision(value: string): value is AdvertisingConsentDecision {
  return value === "unknown" || value === "granted" || value === "denied";
}
