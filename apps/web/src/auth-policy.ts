import type {
  AdminAccessKeyMethod,
  AdminAuthMethod,
  AdminExternalAuthMethod,
  Capabilities,
  StudioAccess,
} from "@opensoverignblog/sdk";

export interface AdminAuthChoices {
  status: "disabled" | "ready" | "misconfigured";
  accessKeyMethods: AdminAccessKeyMethod[];
  externalMethods: AdminExternalAuthMethod[];
}

/** Resolve the v2 control-plane policy while retaining a fail-closed v1 fallback. */
export function studioAccessFor(capabilities: Capabilities): StudioAccess {
  if (capabilities.studioAccess) return capabilities.studioAccess;
  if (capabilities.version.startsWith("2.")) return "disabled";
  if (capabilities.mutationMode === "authenticated_members") return "members";
  return "disabled";
}

export function adminAuthChoices(capabilities: Capabilities): AdminAuthChoices {
  const status = capabilities.auth?.status ?? "disabled";
  const advertisedMethods = capabilities.auth?.methods;
  const methods = status === "ready" && Array.isArray(advertisedMethods)
    ? advertisedMethods.filter(hasSafeAuthAction)
    : [];
  return {
    status,
    accessKeyMethods: methods.filter(isAccessKeyMethod),
    externalMethods: methods.filter(isExternalMethod),
  };
}

export function safeAuthActionHref(method: AdminAuthMethod): string | undefined {
  return hasSafeAuthAction(method) ? normalizedAuthAction(method.actionHref) : undefined;
}

function isAccessKeyMethod(method: AdminAuthMethod): method is AdminAccessKeyMethod {
  return method.kind === "access_key" && method.flow === "secret_exchange";
}

function isExternalMethod(method: AdminAuthMethod): method is AdminExternalAuthMethod {
  return method.kind === "external" && method.flow === "redirect";
}

function hasSafeAuthAction(method: unknown): method is AdminAuthMethod {
  if (typeof method !== "object" || method === null) return false;
  const candidate = method as Record<string, unknown>;
  if (
    candidate.audience !== "admin"
    || typeof candidate.label !== "string"
    || candidate.label.length === 0
    || typeof candidate.actionHref !== "string"
    || normalizedAuthAction(candidate.actionHref) === undefined
  ) return false;
  if (candidate.kind === "access_key") {
    return candidate.id === "admin-access-key" && candidate.flow === "secret_exchange";
  }
  return candidate.kind === "external"
    && candidate.id === "admin-external"
    && candidate.flow === "redirect"
    && typeof candidate.provider === "string"
    && candidate.provider.length > 0;
}

function normalizedAuthAction(value: string): string | undefined {
  if (
    typeof value !== "string"
    || !value.startsWith("/")
    || value.startsWith("//")
    || value.includes("\\")
    || value.includes("#")
  ) return undefined;
  const parsed = new URL(value, "https://open-soverign-blog.invalid");
  if (
    parsed.origin !== "https://open-soverign-blog.invalid"
    || !parsed.pathname.startsWith("/api/v1/auth/")
  ) return undefined;
  return `${parsed.pathname}${parsed.search}`;
}
