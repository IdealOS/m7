// Enough Markdown for the preview: headings, paragraphs, lists, quotes, code,
// rules and images, line by line. Text widgets can't yet mix styles within a run
// of text, so inline marks (emphasis, code, links) are taken out of the text
// they're in, and links are listed with their block so they can be followed.
//
// Two link forms are the system's own: [text](doc:<id>) links to another
// document, and ![alt](blob:<sha-256>) shows an image stored as a blob.

export interface Link {
  text: string;
  /** Where it goes: \`doc:<id>\`, a web address, or anything else written. */
  target: string;
}

export type Block =
  | { kind: "heading"; level: number; text: string; links: Link[] }
  | { kind: "paragraph"; text: string; links: Link[]; strong: boolean }
  | { kind: "item"; marker: string; depth: number; text: string; links: Link[] }
  | { kind: "quote"; text: string; links: Link[] }
  | { kind: "code"; text: string }
  | { kind: "image"; alt: string; blob: string }
  | { kind: "rule" };

const LINK = /!?\[([^\]]*)\]\(([^)\s]*)\)/g;

/** A run of inline Markdown as plain text, and the links in it. */
export function inline(md: string): { text: string; links: Link[]; strong: boolean } {
  const links: Link[] = [];
  const trimmed = md.trim();
  const strong = /^(\*\*|__)[^*_].*\1$/.test(trimmed);
  let text = trimmed.replace(LINK, (whole, label: string, target: string) => {
    if (whole.startsWith("!")) return label;
    links.push({ text: label, target });
    return label;
  });
  text = text
    .replace(/`([^`]*)`/g, "$1")
    .replace(/(\*\*|__)(.+?)\1/g, "$2")
    .replace(/(^|[^\w*])[*_]([^*_\s][^*_]*?)[*_](?=[^\w*]|$)/g, "$1$2");
  return { text, links, strong };
}

/** The blocks of \`md\`, in order. */
export function parse(md: string): Block[] {
  const blocks: Block[] = [];
  const lines = md.replace(/\r\n?/g, "\n").split("\n");
  let para: string[] = [];
  let quote: string[] = [];
  const flush = () => {
    if (para.length > 0) {
      const { text, links, strong } = inline(para.join(" "));
      blocks.push({ kind: "paragraph", text, links, strong });
      para = [];
    }
    if (quote.length > 0) {
      const { text, links } = inline(quote.join(" "));
      blocks.push({ kind: "quote", text, links });
      quote = [];
    }
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^\s*(```|~~~)/.test(line)) {
      flush();
      const fence = line.trim().slice(0, 3);
      const code: string[] = [];
      for (i++; i < lines.length && !lines[i].trim().startsWith(fence); i++) code.push(lines[i]);
      blocks.push({ kind: "code", text: code.join("\n") });
      continue;
    }
    if (line.trim() === "") {
      flush();
      continue;
    }
    const heading = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
    if (heading) {
      flush();
      const { text, links } = inline(heading[2]);
      blocks.push({ kind: "heading", level: heading[1].length, text, links });
      continue;
    }
    if (/^\s{0,3}([-*_])(\s*\1){2,}\s*$/.test(line)) {
      flush();
      blocks.push({ kind: "rule" });
      continue;
    }
    const image = /^\s*!\[([^\]]*)\]\(blob:([0-9a-f]{64})\)\s*$/.exec(line);
    if (image) {
      flush();
      blocks.push({ kind: "image", alt: image[1], blob: image[2] });
      continue;
    }
    const item = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/.exec(line);
    if (item) {
      flush();
      const { text, links } = inline(item[3]);
      const marker = /\d/.test(item[2]) ? item[2].replace(")", ".") : "•";
      blocks.push({ kind: "item", marker, depth: Math.floor(item[1].replace(/\t/g, "  ").length / 2), text, links });
      continue;
    }
    const q = /^\s*>\s?(.*)$/.exec(line);
    if (q) {
      if (para.length > 0) flush();
      quote.push(q[1]);
      continue;
    }
    if (quote.length > 0) flush();
    para.push(line.trim());
  }
  flush();
  return blocks;
}

/** The ids of the documents \`md\` links to, each once, in order. */
export function docLinks(md: string): string[] {
  const ids = [...md.matchAll(/\]\(doc:([0-9a-f-]{36})\)/g)].map((m) => m[1]);
  return [...new Set(ids)];
}

/** The blobs \`md\` shows as images, each once, in order. */
export function blobRefs(md: string): string[] {
  const hashes = [...md.matchAll(/\]\(blob:([0-9a-f]{64})\)/g)].map((m) => m[1]);
  return [...new Set(hashes)];
}
