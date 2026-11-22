// Minimal XML property list writer: strings, integers, booleans, arrays and
// dictionaries, which is all a UTM config.plist uses.

export type PlistValue = string | number | boolean | PlistValue[] | { [key: string]: PlistValue };

function escape(s: string): string {
  return s.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

function value(v: PlistValue, indent: string): string {
  const inner = indent + "\t";
  if (typeof v === "string") return `${indent}<string>${escape(v)}</string>`;
  if (typeof v === "boolean") return `${indent}<${v}/>`;
  if (typeof v === "number") {
    if (!Number.isInteger(v)) throw new Error(`plist: only integers are supported, got ${v}`);
    return `${indent}<integer>${v}</integer>`;
  }
  if (Array.isArray(v)) {
    if (v.length === 0) return `${indent}<array/>`;
    return [`${indent}<array>`, ...v.map((x) => value(x, inner)), `${indent}</array>`].join("\n");
  }
  const keys = Object.keys(v).sort();
  if (keys.length === 0) return `${indent}<dict/>`;
  const body = keys.flatMap((k) => [`${inner}<key>${escape(k)}</key>`, value(v[k], inner)]);
  return [`${indent}<dict>`, ...body, `${indent}</dict>`].join("\n");
}

/** A complete plist document with `root` as its top-level value. */
export function plist(root: PlistValue): string {
  return [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">',
    '<plist version="1.0">',
    value(root, ""),
    "</plist>",
    "",
  ].join("\n");
}
