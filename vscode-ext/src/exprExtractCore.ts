export interface ExtractedChain {
  /** Normalized C/C++ member/index/scope chain ending at the cursor anchor. */
  expr: string;
  /** True when the chain starts with explicit global scope `::`. */
  explicitGlobal: boolean;
  /** Leftmost root identifier/path before `.` / `->` / `[` (without leading `::`). */
  rootPath: string;
  /** Unqualified identifier at the root when `rootPath` contains no `::`. */
  rootIdent: string;
}

const CPP_KEYWORDS = new Set([
  "alignas",
  "alignof",
  "and",
  "and_eq",
  "asm",
  "auto",
  "bitand",
  "bitor",
  "bool",
  "break",
  "case",
  "catch",
  "char",
  "char8_t",
  "char16_t",
  "char32_t",
  "class",
  "compl",
  "concept",
  "const",
  "consteval",
  "constexpr",
  "constinit",
  "const_cast",
  "continue",
  "co_await",
  "co_return",
  "co_yield",
  "decltype",
  "default",
  "delete",
  "do",
  "double",
  "dynamic_cast",
  "else",
  "enum",
  "explicit",
  "export",
  "extern",
  "false",
  "float",
  "for",
  "friend",
  "goto",
  "if",
  "inline",
  "int",
  "long",
  "mutable",
  "namespace",
  "new",
  "noexcept",
  "not",
  "not_eq",
  "nullptr",
  "operator",
  "or",
  "or_eq",
  "private",
  "protected",
  "public",
  "register",
  "reinterpret_cast",
  "requires",
  "return",
  "short",
  "signed",
  "sizeof",
  "static",
  "static_assert",
  "static_cast",
  "struct",
  "switch",
  "template",
  "this",
  "thread_local",
  "throw",
  "true",
  "try",
  "typedef",
  "typeid",
  "typename",
  "union",
  "unsigned",
  "using",
  "virtual",
  "void",
  "volatile",
  "wchar_t",
  "while",
  "xor",
  "xor_eq",
]);

const MAX_SCAN_WINDOW = 512;

function isIdentChar(ch: string | undefined): boolean {
  return ch !== undefined && /[A-Za-z0-9_]/.test(ch);
}

function isIdentStart(ch: string | undefined): boolean {
  return ch !== undefined && /[A-Za-z_]/.test(ch);
}

function isHorizSpace(ch: string | undefined): boolean {
  return ch === " " || ch === "\t";
}

function isSpace(ch: string | undefined): boolean {
  return ch === " " || ch === "\t" || ch === "\r" || ch === "\n";
}

function skipSpacesBack(source: string, pos: number, minPos: number): number {
  let p = pos;
  while (p > minPos && isSpace(source[p - 1])) {
    p--;
  }
  return p;
}

function skipHorizBack(source: string, pos: number, minPos: number): number {
  let p = pos;
  while (p > minPos && isHorizSpace(source[p - 1])) {
    p--;
  }
  return p;
}

/** Check whether `[` at `openIdx` is a postfix array subscript (not `[[attr]]` or a lambda). */
function isSubscriptOpen(source: string, openIdx: number): boolean {
  const left = skipHorizBack(source, openIdx, Math.max(0, openIdx - 64));
  if (left <= 0) {
    return false;
  }
  const ch = source[left - 1];
  return isIdentChar(ch) || ch === "]" || ch === ")";
}

/** Match forward from `[` at `openIdx` to its closing `]`. */
function matchBracketForward(source: string, openIdx: number, maxPos: number): number | undefined {
  let depth = 0;
  let quote: string | null = null;
  for (let i = openIdx; i < maxPos; i++) {
    const ch = source[i];
    if (quote) {
      if (ch === "\\") {
        i++;
      } else if (ch === quote) {
        quote = null;
      }
      continue;
    }
    if (ch === "'" || ch === '"') {
      quote = ch;
    } else if (ch === ";" || ch === "{" || ch === "}") {
      return undefined;
    } else if (ch === "[") {
      depth++;
    } else if (ch === "]") {
      depth--;
      if (depth === 0) {
        return i;
      }
    }
  }
  return undefined;
}

/** Match backward from `]` at `closeIdx` to its opening `[`. */
function matchBracketBackward(source: string, closeIdx: number, minPos: number): number | undefined {
  let depth = 0;
  let quote: string | null = null;
  for (let i = closeIdx; i >= minPos; i--) {
    const ch = source[i];
    if (quote) {
      if (ch === quote && source[i - 1] !== "\\") {
        quote = null;
      }
      continue;
    }
    if (ch === "'" || ch === '"') {
      quote = ch;
    } else if (ch === ";" || ch === "{" || ch === "}") {
      return undefined;
    } else if (ch === "]") {
      depth++;
    } else if (ch === "[") {
      depth--;
      if (depth === 0) {
        return i;
      }
    }
  }
  return undefined;
}

/** Match backward from `)` at `closeIdx` to its opening `(`. */
function matchParenBackward(source: string, closeIdx: number, minPos: number): number | undefined {
  let depth = 0;
  let quote: string | null = null;
  for (let i = closeIdx; i >= minPos; i--) {
    const ch = source[i];
    if (quote) {
      if (ch === quote && source[i - 1] !== "\\") {
        quote = null;
      }
      continue;
    }
    if (ch === "'" || ch === '"') {
      quote = ch;
    } else if (ch === ";" || ch === "{" || ch === "}") {
      return undefined;
    } else if (ch === ")") {
      depth++;
    } else if (ch === "(") {
      depth--;
      if (depth === 0) {
        return i;
      }
    }
  }
  return undefined;
}

/** Match backward from `>` at `closeIdx` to `<` of a template argument list. */
function matchAngleBackward(source: string, closeIdx: number, minPos: number): number | undefined {
  let depth = 0;
  for (let i = closeIdx; i >= minPos; i--) {
    const ch = source[i];
    if (ch === ";" || ch === "{" || ch === "}" || ch === "\n") {
      return undefined;
    }
    if (ch === ">") {
      depth++;
    } else if (ch === "<") {
      depth--;
      if (depth === 0) {
        return i;
      }
    }
  }
  return undefined;
}

/** Find the outermost subscript closing `]` if `cursorOffset` lies inside `[...]`. */
function findOutermostEnclosingSubscriptEnd(
  source: string,
  cursorOffset: number,
  minPos: number,
  maxPos: number,
): number | undefined {
  let depth = 0;
  let outermostEnd: number | undefined;
  let quote: string | null = null;

  for (let i = cursorOffset - 1; i >= minPos; i--) {
    const ch = source[i];
    if (quote) {
      if (ch === quote && source[i - 1] !== "\\") {
        quote = null;
      }
      continue;
    }
    if (ch === "'" || ch === '"') {
      quote = ch;
    } else if (ch === ";" || ch === "{" || ch === "}" || ch === "\n") {
      break;
    } else if (ch === "]") {
      depth++;
    } else if (ch === "[") {
      if (depth > 0) {
        depth--;
      } else {
        if (!isSubscriptOpen(source, i)) {
          break;
        }
        const closeIdx = matchBracketForward(source, i, maxPos);
        if (closeIdx === undefined || closeIdx < cursorOffset) {
          break;
        }
        outermostEnd = closeIdx + 1;
      }
    }
  }
  return outermostEnd;
}

function identifierAtCursor(source: string, cursorOffset: number): { start: number; end: number } | undefined {
  let start = cursorOffset;
  let end = cursorOffset;
  if (cursorOffset < source.length && isIdentChar(source[cursorOffset])) {
    while (start > 0 && isIdentChar(source[start - 1])) {
      start--;
    }
    while (end < source.length && isIdentChar(source[end])) {
      end++;
    }
  } else if (cursorOffset > 0 && isIdentChar(source[cursorOffset - 1])) {
    start = cursorOffset - 1;
    while (start > 0 && isIdentChar(source[start - 1])) {
      start--;
    }
  } else {
    return undefined;
  }

  const text = source.slice(start, end);
  if (!isIdentStart(text[0]) || CPP_KEYWORDS.has(text)) {
    return undefined;
  }
  return { start, end };
}

interface ConsumedPath {
  start: number;
  explicitGlobal: boolean;
}

function consumeIdentBack(source: string, end: number, minPos: number): number | undefined {
  let start = end;
  while (start > minPos && isIdentChar(source[start - 1])) {
    start--;
  }
  if (start === end || !isIdentStart(source[start])) {
    return undefined;
  }
  return start;
}

function consumePathBack(source: string, end: number, minPos: number): ConsumedPath | undefined {
  const firstStart = consumeIdentBack(source, end, minPos);
  if (firstStart === undefined) {
    return undefined;
  }
  let pos = firstStart;
  let explicitGlobal = false;

  while (pos >= minPos + 2) {
    const beforeSpaces = skipSpacesBack(source, pos, minPos);
    if (beforeSpaces < minPos + 2 || source.slice(beforeSpaces - 2, beforeSpaces) !== "::") {
      break;
    }
    const colonStart = beforeSpaces - 2;
    const left = skipSpacesBack(source, colonStart, minPos);
    if (left > minPos && source[left - 1] === ">") {
      const angleOpen = matchAngleBackward(source, left - 1, minPos);
      if (angleOpen === undefined) {
        break;
      }
      const tmplIdentStart = consumeIdentBack(source, skipSpacesBack(source, angleOpen, minPos), minPos);
      if (tmplIdentStart === undefined) {
        break;
      }
      pos = tmplIdentStart;
      continue;
    }
    const prevIdentStart = consumeIdentBack(source, left, minPos);
    if (prevIdentStart !== undefined) {
      pos = prevIdentStart;
      continue;
    }
    // Leading global `::`.
    if (left === 0 || !isIdentChar(source[left - 1])) {
      pos = colonStart;
      explicitGlobal = true;
    }
    break;
  }

  return { start: pos, explicitGlobal };
}

function normalizeChainWhitespace(raw: string): string {
  return raw
    .replace(/\s*\r?\n\s*/g, " ")
    .replace(/\s*(::|->|\.)\s*/g, "$1")
    .replace(/\s*\[\s*/g, "[")
    .replace(/\s*\]/g, "]")
    .replace(/\(\s*\*\s*/g, "(*")
    .replace(/\s*\)/g, ")")
    .replace(/\s{2,}/g, " ")
    .trim();
}

function extractRootPath(normalizedExpr: string): { rootPath: string; rootIdent: string } {
  const stripped = normalizedExpr.startsWith("::") ? normalizedExpr.slice(2) : normalizedExpr;
  let depthAngle = 0;
  let depthParen = 0;
  let end = stripped.length;
  for (let i = 0; i < stripped.length; i++) {
    const ch = stripped[i];
    if (ch === "<") depthAngle++;
    else if (ch === ">" && depthAngle > 0) depthAngle--;
    else if (ch === "(") depthParen++;
    else if (ch === ")" && depthParen > 0) depthParen--;
    else if (depthAngle === 0 && depthParen === 0) {
      if (ch === "." || ch === "[" || (ch === "-" && stripped[i + 1] === ">")) {
        end = i;
        break;
      }
    }
  }
  const rootPath = stripped.slice(0, end);
  const parts = splitQualifiedSymbolName(rootPath);
  const rootIdent = parts[parts.length - 1] ?? rootPath;
  return { rootPath, rootIdent };
}

/**
 * Extract a C/C++ member/index/scope chain ending at the cursor position by walking
 * left along `.`, `->`, `::`, and `[...]` subscripts without crossing unary `*` / `&`
 * or expanding right past the clicked segment (except snapping outward from inside `[...]`).
 */
export function extractLeftChainAtOffset(
  source: string,
  cursorOffset: number,
): ExtractedChain | undefined {
  if (cursorOffset < 0 || cursorOffset > source.length) {
    return undefined;
  }
  const minPos = Math.max(0, cursorOffset - MAX_SCAN_WINDOW);
  const maxPos = Math.min(source.length, cursorOffset + MAX_SCAN_WINDOW);

  // Q1: When the cursor sits inside `[...]`, snap outward to the outermost enclosing `]`.
  // Also reject C++ `[[attribute]]` spans.
  let anchorEnd = findOutermostEnclosingSubscriptEnd(source, cursorOffset, minPos, maxPos);

  if (anchorEnd === undefined) {
    // Check if inside a C++20 `[[...]]` attribute.
    const lineStart = source.lastIndexOf("\n", cursorOffset - 1) + 1;
    const beforeOnLine = source.slice(lineStart, cursorOffset);
    const openAttr = beforeOnLine.lastIndexOf("[[");
    if (openAttr >= 0 && !beforeOnLine.slice(openAttr).includes("]]")) {
      return undefined;
    }

    if (cursorOffset < source.length && source[cursorOffset] === "]") {
      anchorEnd = cursorOffset + 1;
    } else if (cursorOffset > 0 && source[cursorOffset - 1] === "]") {
      anchorEnd = cursorOffset;
    } else {
      const ident = identifierAtCursor(source, cursorOffset);
      if (ident) {
        anchorEnd = ident.end;
      } else if (
        cursorOffset < source.length &&
        source[cursorOffset] === "[" &&
        isSubscriptOpen(source, cursorOffset)
      ) {
        const closeIdx = matchBracketForward(source, cursorOffset, maxPos);
        if (closeIdx !== undefined) {
          anchorEnd = closeIdx + 1;
        }
      }
    }
  }

  if (anchorEnd === undefined) {
    return undefined;
  }

  let chainStart = anchorEnd;
  let explicitGlobal = false;
  let pos = anchorEnd;

  while (pos > minPos) {
    // 1. Consume any trailing `[...]` subscripts belonging to the current link.
    while (pos > minPos && source[pos - 1] === "]") {
      const openIdx = matchBracketBackward(source, pos - 1, minPos);
      if (openIdx === undefined || !isSubscriptOpen(source, openIdx)) {
        return undefined;
      }
      chainStart = openIdx;
      pos = skipSpacesBack(source, openIdx, minPos);
    }

    // 2. Consume the base segment at `pos`: either `(*expr)` or a `ns::Type<T>::ident` path.
    if (pos > minPos && source[pos - 1] === ")") {
      const openParen = matchParenBackward(source, pos - 1, minPos);
      if (openParen === undefined) {
        return undefined;
      }
      const beforeParen = skipSpacesBack(source, openParen, minPos);
      // Function call `foo().x` or C++ cast `static_cast<T>(x).y` cannot be evaluated on target.
      if (beforeParen > minPos && (isIdentChar(source[beforeParen - 1]) || source[beforeParen - 1] === ">")) {
        return undefined;
      }
      const inner = source.slice(openParen + 1, pos - 1).trim();
      if (!inner || /[;=,{}]/.test(inner)) {
        return undefined;
      }
      chainStart = openParen;
      pos = openParen;
      break;
    }

    const pathRes = consumePathBack(source, pos, minPos);
    if (!pathRes) {
      return undefined;
    }
    const leafText = source.slice(pathRes.start, pos);
    if (!leafText.includes("::") && CPP_KEYWORDS.has(leafText)) {
      return undefined;
    }
    chainStart = pathRes.start;
    explicitGlobal = pathRes.explicitGlobal;
    pos = pathRes.start;

    if (explicitGlobal) {
      break;
    }

    // 3. Check for `.` or `->` immediately to the left of this path.
    const opEnd = skipSpacesBack(source, pos, minPos);
    if (opEnd >= minPos + 2 && source.slice(opEnd - 2, opEnd) === "->") {
      if (opEnd < source.length && source[opEnd] === "*") {
        break;
      }
      pos = skipSpacesBack(source, opEnd - 2, minPos);
      continue;
    }
    if (opEnd >= minPos + 1 && source[opEnd - 1] === ".") {
      const prevChar = opEnd >= 2 ? source[opEnd - 2] : undefined;
      if (prevChar === "." || (prevChar !== undefined && /\d/.test(prevChar))) {
        break;
      }
      pos = skipSpacesBack(source, opEnd - 1, minPos);
      continue;
    }
    break;
  }

  const rawSlice = source.slice(chainStart, anchorEnd);
  const expr = normalizeChainWhitespace(rawSlice);
  if (!expr) {
    return undefined;
  }
  const { rootPath, rootIdent } = extractRootPath(expr);
  if (!rootIdent || CPP_KEYWORDS.has(rootIdent)) {
    return undefined;
  }
  return {
    expr,
    explicitGlobal,
    rootPath,
    rootIdent,
  };
}

const ANONYMOUS_NAMES = new Set([
  "",
  "(anonymous namespace)",
  "anonymous namespace",
  "(anonymous)",
  "<anonymous>",
  "<unnamed>",
  "__anon",
]);

function stripTrailingSignature(rawName: string): string {
  let text = rawName.trim();
  // Strip optional trailing method qualifiers (`const`, `volatile`, `noexcept`, `&`, `&&`, `override`, `final`).
  text = text.replace(/(?:\s+(?:const|volatile|noexcept(?:\([^)]*\))?|override|final)|\s*&{1,2})+\s*$/, "").trim();
  if (!text.endsWith(")")) {
    return rawName.trim();
  }
  const openParen = matchParenBackward(text, text.length - 1, 0);
  if (openParen === undefined || openParen === 0) {
    return rawName.trim();
  }
  return text.slice(0, openParen).trim();
}

/**
 * Split a `DocumentSymbol` name such as `ConceptArm::LeftArm::tick()` or `foc::axis0`
 * into clean namespace/class/function segments while ignoring template `::` and anonymous scopes.
 */
export function splitQualifiedSymbolName(rawName: string): string[] {
  const text = stripTrailingSignature(rawName);
  if (!text) {
    return [];
  }

  const segments: string[] = [];
  let start = 0;
  let depthAngle = 0;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (ch === "<") {
      depthAngle++;
    } else if (ch === ">" && depthAngle > 0) {
      depthAngle--;
    } else if (depthAngle === 0 && ch === ":" && text[i + 1] === ":") {
      const part = text.slice(start, i).trim();
      if (!ANONYMOUS_NAMES.has(part.toLowerCase()) && !part.startsWith("(")) {
        segments.push(part);
      }
      i++;
      start = i + 1;
    }
  }
  const tail = text.slice(start).trim();
  if (tail && !ANONYMOUS_NAMES.has(tail.toLowerCase()) && !tail.startsWith("(")) {
    segments.push(tail);
  }
  return segments;
}

/**
 * Generate ordered candidate expressions from innermost enclosing C++ scope
 * outward to the raw unqualified expression.
 */
export function buildScopedCandidates(
  rawExpr: string,
  explicitGlobal: boolean,
  scopeChain: string[],
): string[] {
  const trimmed = rawExpr.trim();
  if (!trimmed) {
    return [];
  }
  if (explicitGlobal || trimmed.startsWith("::")) {
    return [trimmed];
  }
  const out: string[] = [];
  const seen = new Set<string>();
  const add = (candidate: string) => {
    if (!seen.has(candidate)) {
      seen.add(candidate);
      out.push(candidate);
    }
  };

  for (let len = scopeChain.length; len >= 1; len--) {
    const prefix = scopeChain.slice(0, len).join("::");
    if (!prefix) {
      continue;
    }
    if (trimmed === prefix || trimmed.startsWith(`${prefix}::`)) {
      continue;
    }
    add(`${prefix}::${trimmed}`);
  }
  add(trimmed);
  return out;
}
