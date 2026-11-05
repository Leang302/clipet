// History search: one query matched against an item's own content and the
// text recognized inside it (OCR). Plain substring matching works for Korean
// (매출 finds 매출관리내역), unlike word-tokenized full-text search.

export function normalize(text: string): string {
  // NFC: a decomposed query (e.g. pasted from a file name) still matches the
  // precomposed syllables OCR produces. Lowercase: English is case-insensitive.
  return text.normalize("NFC").toLowerCase();
}

const compact = (text: string) => text.replace(/\s+/g, "");

export type SearchMatch = {
  /** 0 exact, 1 substring, 2 substring ignoring spaces (OCR), 3 all words found. Lower ranks sort first. */
  rank: number;
  /** True when the match came only from text inside the image. */
  viaOcr: boolean;
};

/** How (and whether) an item matches a query. `null` when it doesn't. */
export function matchItem(query: string, content: string, ocrText: string | null): SearchMatch | null {
  const q = normalize(query).trim();
  if (!q) return { rank: 0, viaOcr: false };
  const c = normalize(content);
  const o = normalize(ocrText ?? "");
  const oLines = o.split("\n").map((l) => l.trim());

  if (c.trim() === q) return { rank: 0, viaOcr: false };
  if (oLines.includes(q)) return { rank: 0, viaOcr: true };
  if (c.includes(q)) return { rank: 1, viaOcr: false };
  if (o.includes(q)) return { rank: 1, viaOcr: true };
  // OCR may insert or drop spaces ("Null PointerException", "계약등록").
  const cq = compact(q);
  if (o && compact(o).includes(cq)) return { rank: 2, viaOcr: true };

  const terms = q.split(/\s+/);
  if (terms.length > 1) {
    let viaOcr = false;
    for (const term of terms) {
      if (c.includes(term)) continue;
      if (o && compact(o).includes(term)) { viaOcr = true; continue; }
      return null;
    }
    return { rank: 3, viaOcr };
  }
  return null;
}

/** Whether one recognized line should be highlighted for this query. */
export function lineMatches(query: string, line: string): boolean {
  const q = normalize(query).trim();
  if (!q) return false;
  const l = normalize(line);
  if (l.includes(q) || compact(l).includes(compact(q))) return true;
  const terms = q.split(/\s+/).filter((t) => t.length > 1);
  return terms.length > 1 && terms.some((t) => compact(l).includes(t));
}
