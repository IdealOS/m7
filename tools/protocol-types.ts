// TypeScript declarations from the protocol's JSON Schema (crates/protocol/schema.json).
//
// Handles the subset of JSON Schema that schemars produces for the protocol crate,
// and fails on anything else, so a new kind of Rust type can't silently turn into
// `unknown`. tools/gen-protocol.ts is the command; this file is pure for testing.

export type Schema = boolean | { [key: string]: unknown };

/** Keywords that only describe or constrain values; TypeScript can't express them. */
const ANNOTATIONS = new Set(["description", "default", "format", "minimum", "maximum", "title", "minItems", "maxItems"]);
/** Keywords this generator turns into types. */
const HANDLED = new Set([
  "$ref", "const", "enum", "oneOf", "anyOf", "type", "properties", "required", "items", "prefixItems", "additionalProperties",
]);

const MAX_LINE = 100;

export class UnsupportedSchema extends Error {}

function isObject(s: unknown): s is Record<string, unknown> {
  return typeof s === "object" && s !== null && !Array.isArray(s);
}

function fail(path: string, why: string): never {
  throw new UnsupportedSchema(`${path}: ${why}`);
}

/** Wrap `text` at word boundaries so each line fits in `width` columns. */
export function wrap(text: string, width: number): string[] {
  const lines: string[] = [];
  for (const para of text.split("\n")) {
    let line = "";
    for (const word of para.split(" ")) {
      if (line && line.length + 1 + word.length > width) {
        lines.push(line);
        line = word;
      } else {
        line = line ? `${line} ${word}` : word;
      }
    }
    lines.push(line);
  }
  return lines;
}

/** `/** text *\/`, on one line when it fits, indented by `indent`. */
export function docComment(text: unknown, indent = ""): string {
  if (typeof text !== "string" || text.trim() === "") return "";
  const clean = text.trim().replaceAll("*/", "*\\/");
  if (!clean.includes("\n") && indent.length + clean.length + 7 <= MAX_LINE) {
    return `${indent}/** ${clean} */\n`;
  }
  const lines = wrap(clean, MAX_LINE - indent.length - 3);
  return `${indent}/**\n${lines.map((l) => `${indent} *${l ? " " + l : ""}`).join("\n")}\n${indent} */\n`;
}

const IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

/** The TypeScript type for `schema`, laid out to start at column `indent`. */
export function typeOf(schema: Schema, path: string, indent = ""): string {
  if (schema === true) return "unknown";
  if (schema === false) return "never";
  if (!isObject(schema)) fail(path, "not a schema");
  for (const key of Object.keys(schema)) {
    if (!HANDLED.has(key) && !ANNOTATIONS.has(key)) fail(path, `unsupported keyword "${key}"`);
  }

  if (typeof schema.$ref === "string") {
    const m = /^#\/\$defs\/(.+)$/.exec(schema.$ref);
    if (!m || !IDENT.test(m[1])) fail(path, `unsupported $ref ${schema.$ref}`);
    return m[1];
  }
  if ("const" in schema) return JSON.stringify(schema.const);
  if (Array.isArray(schema.enum)) return schema.enum.map((v) => JSON.stringify(v)).join(" | ");
  const variants = schema.oneOf ?? schema.anyOf;
  if (variants !== undefined) {
    if (!Array.isArray(variants)) fail(path, "oneOf/anyOf must be a list");
    const either = union(variants as Schema[], `${path}/oneOf`, indent);
    // Fields of its own beside the variants (a Rust struct that flattens an
    // enum): both.
    if ("properties" in schema) {
      const own = objectType({ ...schema, oneOf: undefined, anyOf: undefined, type: "object" }, path, indent);
      return `${own} & (${either})`;
    }
    return either;
  }

  const type = schema.type;
  if (Array.isArray(type)) {
    // `["integer", "null"]`: the same schema once per type.
    return type.map((t, i) => typeOf({ ...schema, type: t }, `${path}/type/${i}`, indent)).join(" | ");
  }
  switch (type) {
    case undefined:
      // No type and no other constraint: any JSON value (serde_json::Value).
      if ("properties" in schema || "items" in schema) fail(path, "properties or items without a type");
      return "unknown";
    case "string":
      return "string";
    case "integer":
    case "number":
      return "number";
    case "boolean":
      return "boolean";
    case "null":
      return "null";
    case "array": {
      if (Array.isArray(schema.prefixItems)) {
        // A tuple, as schemars writes a Rust tuple struct: exactly these items.
        if (schema.items !== undefined && schema.items !== false) fail(path, "a tuple with extra items");
        const items = schema.prefixItems.map((s, i) => typeOf(s as Schema, `${path}/prefixItems/${i}`, indent));
        return `[${items.join(", ")}]`;
      }
      if (schema.items === undefined) return "unknown[]";
      const item = typeOf(schema.items as Schema, `${path}/items`, indent);
      return /^[A-Za-z0-9_$"]+$/.test(item) ? `${item}[]` : `Array<${item}>`;
    }
    case "object":
      return objectType(schema, path, indent);
    default:
      return fail(path, `unsupported type ${JSON.stringify(type)}`);
  }
}

function union(variants: Schema[], path: string, indent: string): string {
  const parts = variants.map((v, i) => ({ doc: isObject(v) ? v.description : undefined, type: typeOf(v, `${path}/${i}`, indent + "  ") }));
  const oneLine = parts.map((p) => p.type).join(" | ");
  if (parts.every((p) => !p.doc && !p.type.includes("\n")) && indent.length + oneLine.length <= MAX_LINE) {
    return oneLine;
  }
  return parts.map((p) => `\n${docComment(p.doc, indent + "  ")}${indent}  | ${p.type}`).join("");
}

function objectType(schema: Record<string, unknown>, path: string, indent: string): string {
  const props = schema.properties;
  const extra = schema.additionalProperties;
  if (props === undefined) {
    if (extra === undefined || extra === true) return "Record<string, unknown>";
    if (extra === false) return "Record<string, never>";
    return `Record<string, ${typeOf(extra as Schema, `${path}/additionalProperties`, indent)}>`;
  }
  if (!isObject(props)) fail(path, "properties must be an object");
  if (extra !== undefined && extra !== false) fail(path, "properties with additionalProperties");
  const required = Array.isArray(schema.required) ? (schema.required as string[]) : [];
  // Required fields in declaration order (schemars keeps it there), then the rest.
  const names = [...required, ...Object.keys(props).filter((k) => !required.includes(k)).sort()];
  const fields = names.map((name) => {
    if (!(name in props)) fail(path, `required field ${name} has no schema`);
    const key = IDENT.test(name) ? name : JSON.stringify(name);
    const sub = props[name] as Schema;
    return {
      doc: isObject(sub) ? sub.description : undefined,
      text: `${key}${required.includes(name) ? "" : "?"}: ${typeOf(sub, `${path}/properties/${name}`, indent + "  ")}`,
    };
  });
  const oneLine = `{ ${fields.map((f) => f.text).join("; ")} }`;
  if (fields.every((f) => !f.doc && !f.text.includes("\n")) && indent.length + oneLine.length <= MAX_LINE) {
    return oneLine;
  }
  const body = fields.map((f) => `${docComment(f.doc, indent + "  ")}${indent}  ${f.text};`).join("\n");
  return `{\n${body}\n${indent}}`;
}

/** The whole generated file for the protocol schema document. */
export function generate(doc: unknown, source: string): string {
  if (!isObject(doc)) fail("#", "not a schema document");
  const header = [
    `Generated by tools/gen-protocol.ts from ${source}, which crates/protocol-schema exports from the Rust types in crates/protocol.`,
    "Do not edit: run `make protocol` after changing the Rust types.",
    "",
    ...(typeof doc.description === "string" ? [doc.description, ""] : []),
    "Integers are 64-bit on the wire; ids and counts stay well below 2^53.",
  ];
  const out = [...wrap(header.join("\n"), MAX_LINE - 3).map((l) => (l ? `// ${l}` : "//")), ""];

  const constants = doc["x-constants"];
  if (constants !== undefined) {
    if (!isObject(constants)) fail("#/x-constants", "not an object");
    for (const [name, c] of Object.entries(constants)) {
      if (!IDENT.test(name) || !isObject(c) || !("value" in c)) fail(`#/x-constants/${name}`, "expected {value, description}");
      out.push(`${docComment(c.description)}export const ${name} = ${JSON.stringify(c.value)};`, "");
    }
  }

  const defs = doc.$defs;
  if (!isObject(defs)) fail("#/$defs", "missing");
  for (const [name, schema] of Object.entries(defs)) {
    if (!IDENT.test(name)) fail(`#/$defs/${name}`, "not a TypeScript name");
    const s = schema as Schema;
    const type = typeOf(s, `#/$defs/${name}`);
    const doc = isObject(s) ? docComment(s.description) : "";
    // A union laid out one variant per line starts with a newline already.
    out.push(`${doc}export type ${name} =${type.startsWith("\n") ? "" : " "}${type};`, "");
  }
  return out.join("\n");
}
