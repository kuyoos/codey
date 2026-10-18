export type ReleaseNoteGroup = { title: string; level: number; lines: string[] };

// 保留版本标题和旧分类标题，正文仍按现有条目方式展示。
export function parseReleaseNotes(notes?: string): ReleaseNoteGroup[] {
  const groups: ReleaseNoteGroup[] = [];
  let group: ReleaseNoteGroup = { title: "", level: 0, lines: [] };
  let headingLevel = 0;
  groups.push(group);
  for (const raw of notes?.split(/\r?\n/) ?? []) {
    const line = raw.trim();
    if (!line) continue;
    const heading = /^(#{1,6})(?:[ \t]+(.*)|$)/.exec(line);
    const category = /^\*\*(新增功能|体验优化|问题修复)\*\*$/.exec(line);
    if (heading || category) {
      if (heading) headingLevel = heading[1].length;
      const rawTitle = heading
        ? (heading[2] ?? "").replace(/(?:[ \t]+|^)#+[ \t]*$/, "").trim()
        : category![1];
      const title = rawTitle.replace(/^\*\*(新增功能|体验优化|问题修复)\*\*$/, "$1");
      group = { title, level: heading ? headingLevel : Math.min(headingLevel + 1, 6), lines: [] };
      groups.push(group);
    } else {
      group.lines.push(line.replace(/^[-*•]\s+/, ""));
    }
  }
  return groups.filter((item) => item.title || item.lines.length > 0);
}
