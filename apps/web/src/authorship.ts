import type { PublicAuthorship } from "@opensoverignblog/sdk";

export const HUMAN_AUTHORSHIP: PublicAuthorship = {
  kind: "human",
  humanReviewed: false,
};

export function normalizedAuthorship(value?: PublicAuthorship): PublicAuthorship {
  if (!value) return HUMAN_AUTHORSHIP;
  return value;
}

export function authorshipLabel(value?: PublicAuthorship, language: "ko" | "en" = "ko"): string {
  const text = (ko: string, en: string) => language === "en" ? en : ko;
  const authorship = normalizedAuthorship(value);
  const review = authorship.humanReviewed ? text(" · 사람 검토", " · human reviewed") : "";
  const generator = authorship.generator?.trim()
    ? ` · ${authorship.generator.trim()}`
    : "";
  switch (authorship.kind) {
    case "ai_generated":
      return `${text("AI 생성", "AI generated")}${generator}${review}`;
    case "ai_assisted":
      return `${text("AI 보조", "AI assisted")}${generator}${review}`;
    case "imported":
      return `${text("가져온 글", "Imported")}${review}`;
    case "human":
    default:
      return text("사람이 작성", "Human authored");
  }
}
