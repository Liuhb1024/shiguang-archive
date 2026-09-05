export function contentEvidence(content?: string): { possibleSummary: boolean; characters: number } {
  const text = content?.trimEnd() || "";
  return { possibleSummary: /(?:\.{3}|…)$/u.test(text), characters: Array.from(content || "").length };
}
