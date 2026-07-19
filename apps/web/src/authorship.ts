import type { PublicAuthorship } from "@opensoverignblog/sdk";

export const HUMAN_AUTHORSHIP: PublicAuthorship = {
  kind: "human",
  humanReviewed: false,
};

export function normalizedAuthorship(value?: PublicAuthorship): PublicAuthorship {
  if (!value) return HUMAN_AUTHORSHIP;
  return value;
}

export function authorshipLabel(value?: PublicAuthorship): string {
  const authorship = normalizedAuthorship(value);
  const review = authorship.humanReviewed ? " · 사람 검토" : "";
  const generator = authorship.generator?.trim()
    ? ` · ${authorship.generator.trim()}`
    : "";
  switch (authorship.kind) {
    case "ai_generated":
      return `AI 생성${generator}${review}`;
    case "ai_assisted":
      return `AI 보조${generator}${review}`;
    case "imported":
      return `가져온 글${review}`;
    case "human":
    default:
      return "사람이 작성";
  }
}
