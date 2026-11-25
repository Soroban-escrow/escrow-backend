/** Acceptance criteria from an issue body (BUILD_SPEC 12.2). Empty list means "derive with AI". */
export function parseCriteria(body: string | null | undefined): string[] {
  const text = (body ?? '').replace(/\r\n/g, '\n');
  const lines = text.split('\n');
  const item = /^\s*(?:[-*+]|\d+[.)])\s+(?:\[[ xX]\]\s+)?(.+?)\s*$/;

  const h = lines.findIndex((l) => /^#{1,6}\s*acceptance criteria\s*:?\s*$/i.test(l.trim()));
  if (h >= 0) {
    const out: string[] = [];
    for (const l of lines.slice(h + 1)) {
      if (/^#{1,6}\s/.test(l)) break;
      const m = item.exec(l);
      if (m?.[1]) out.push(m[1]);
    }
    if (out.length) return out;
  }
  const boxes: string[] = [];
  for (const l of lines) {
    const m = /^\s*[-*+]\s+\[[ xX]\]\s+(.+?)\s*$/.exec(l);
    if (m?.[1]) boxes.push(m[1]);
  }
  return boxes;
}
