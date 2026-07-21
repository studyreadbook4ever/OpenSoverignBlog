export const REFERENCES_PATH = "/references";

export function isReferencesPath(pathname: string): boolean {
  return pathname === REFERENCES_PATH || pathname === `${REFERENCES_PATH}/`;
}
