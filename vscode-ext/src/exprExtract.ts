import * as vscode from "vscode";
import type { DataClient } from "./dataClient";
import {
  buildScopedCandidates,
  extractLeftChainAtOffset,
  splitQualifiedSymbolName,
} from "./exprExtractCore";

const SYMBOL_PROVIDER_TIMEOUT_MS = 400;
const RESOLVE_RPC_TIMEOUT_MS = 500;

const C_FAMILY_LANGUAGES = new Set(["c", "cpp", "cuda-cpp", "objective-c", "objective-cpp"]);

const SCOPE_SYMBOL_KINDS = new Set<vscode.SymbolKind>([
  vscode.SymbolKind.Namespace,
  vscode.SymbolKind.Class,
  vscode.SymbolKind.Struct,
  vscode.SymbolKind.Module,
  vscode.SymbolKind.Function,
  vscode.SymbolKind.Method,
  vscode.SymbolKind.Constructor,
]);

const VARIABLE_SYMBOL_KINDS = new Set<vscode.SymbolKind>([
  vscode.SymbolKind.Variable,
  vscode.SymbolKind.Constant,
  vscode.SymbolKind.Field,
  vscode.SymbolKind.Property,
]);

function withTimeout<T>(promise: Thenable<T> | Promise<T>, timeoutMs: number, fallback: T): Promise<T> {
  return new Promise<T>((resolve) => {
    let settled = false;
    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        resolve(fallback);
      }
    }, timeoutMs);
    Promise.resolve(promise).then(
      (value) => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          resolve(value);
        }
      },
      () => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          resolve(fallback);
        }
      },
    );
  });
}

function rangeSpanSize(range: vscode.Range): number {
  return (range.end.line - range.start.line) * 10000 + (range.end.character - range.start.character);
}

function isDocumentSymbol(
  sym: vscode.DocumentSymbol | vscode.SymbolInformation,
): sym is vscode.DocumentSymbol {
  return "children" in sym && "selectionRange" in sym;
}

interface EnclosingScopeInfo {
  scopeSegments: string[];
  offlinePrefixMatch: string | undefined;
}

function appendUniqueOverlap(target: string[], additions: string[]): void {
  if (additions.length === 0) {
    return;
  }
  // If `clangd` reports `Namespace("ConceptArm")` containing `Method("ConceptArm::LeftArm::tick")`,
  // avoid duplicating `"ConceptArm"` at the boundary.
  let overlap = Math.min(target.length, additions.length);
  while (overlap > 0) {
    let matches = true;
    for (let i = 0; i < overlap; i++) {
      if (target[target.length - overlap + i] !== additions[i]) {
        matches = false;
        break;
      }
    }
    if (matches) {
      break;
    }
    overlap--;
  }
  target.push(...additions.slice(overlap));
}

function collectFromDocumentSymbols(
  symbols: vscode.DocumentSymbol[],
  position: vscode.Position,
  rootIdent: string,
): EnclosingScopeInfo {
  const chain: vscode.DocumentSymbol[] = [];
  const walk = (nodes: vscode.DocumentSymbol[]) => {
    for (const node of nodes) {
      if (!node.range.contains(position)) {
        continue;
      }
      if (SCOPE_SYMBOL_KINDS.has(node.kind)) {
        chain.push(node);
      }
      if (node.children.length > 0) {
        walk(node.children);
      }
    }
  };
  walk(symbols);
  chain.sort((a, b) => rangeSpanSize(b.range) - rangeSpanSize(a.range));

  const scopeSegments: string[] = [];
  let offlinePrefixMatch: string | undefined;

  for (const node of chain) {
    const parts = splitQualifiedSymbolName(node.name);
    appendUniqueOverlap(scopeSegments, parts);
    const hasVar = node.children.some(
      (child) => VARIABLE_SYMBOL_KINDS.has(child.kind) && child.name === rootIdent,
    );
    if (hasVar && scopeSegments.length > 0) {
      offlinePrefixMatch = scopeSegments.join("::");
    }
  }
  return { scopeSegments, offlinePrefixMatch };
}

function collectFromSymbolInfo(
  symbols: vscode.SymbolInformation[],
  position: vscode.Position,
): EnclosingScopeInfo {
  const containing = symbols
    .filter((sym) => SCOPE_SYMBOL_KINDS.has(sym.kind) && sym.location.range.contains(position))
    .sort((a, b) => rangeSpanSize(b.location.range) - rangeSpanSize(a.location.range));

  const scopeSegments: string[] = [];
  for (const sym of containing) {
    if (sym.containerName) {
      appendUniqueOverlap(scopeSegments, splitQualifiedSymbolName(sym.containerName));
    }
    appendUniqueOverlap(scopeSegments, splitQualifiedSymbolName(sym.name));
  }
  return { scopeSegments, offlinePrefixMatch: undefined };
}

async function queryEnclosingScope(
  document: vscode.TextDocument,
  position: vscode.Position,
  rootIdent: string,
): Promise<EnclosingScopeInfo> {
  if (!C_FAMILY_LANGUAGES.has(document.languageId)) {
    return { scopeSegments: [], offlinePrefixMatch: undefined };
  }
  const raw = await withTimeout(
    vscode.commands.executeCommand<Array<vscode.DocumentSymbol | vscode.SymbolInformation> | undefined>(
      "vscode.executeDocumentSymbolProvider",
      document.uri,
    ),
    SYMBOL_PROVIDER_TIMEOUT_MS,
    undefined,
  );
  if (!Array.isArray(raw) || raw.length === 0) {
    return { scopeSegments: [], offlinePrefixMatch: undefined };
  }
  if (isDocumentSymbol(raw[0])) {
    return collectFromDocumentSymbols(raw as vscode.DocumentSymbol[], position, rootIdent);
  }
  return collectFromSymbolInfo(raw as vscode.SymbolInformation[], position);
}

interface ResolveReply {
  resolved: string;
  matched: boolean;
}

/**
 * Extract an expression from the active editor's selection or cursor position:
 * - Non-empty selections are returned verbatim.
 * - Empty selections expand left along `.` / `->` / `::` / `[i]`, query
 *   `vscode.executeDocumentSymbolProvider` for enclosing C++ scopes, and resolve
 *   the ordered candidates against the active `SymbolDb` via `symbols.resolve`.
 */
export async function resolveEditorExpression(
  editor: vscode.TextEditor,
  client: DataClient,
): Promise<string | undefined> {
  if (!editor.selection.isEmpty) {
    const selected = editor.document.getText(editor.selection).trim();
    if (selected) {
      return selected;
    }
  }

  const position = editor.selection.active;
  const offset = editor.document.offsetAt(position);
  const extracted = extractLeftChainAtOffset(editor.document.getText(), offset);
  if (!extracted) {
    return undefined;
  }

  if (extracted.explicitGlobal) {
    if (client.connected) {
      const reply = await withTimeout(
        client.call<ResolveReply>("symbols.resolve", { candidates: [extracted.expr] }),
        RESOLVE_RPC_TIMEOUT_MS,
        { resolved: extracted.expr, matched: false },
      );
      return reply.resolved || extracted.expr;
    }
    return extracted.expr;
  }

  const scope = await queryEnclosingScope(editor.document, position, extracted.rootIdent);
  const candidates = buildScopedCandidates(extracted.expr, false, scope.scopeSegments);

  if (client.connected && candidates.length > 0) {
    const reply = await withTimeout(
      client.call<ResolveReply>("symbols.resolve", { candidates }),
      RESOLVE_RPC_TIMEOUT_MS,
      { resolved: extracted.expr, matched: false },
    );
    return reply.resolved || extracted.expr;
  }

  if (
    scope.offlinePrefixMatch &&
    !extracted.expr.startsWith(`${scope.offlinePrefixMatch}::`) &&
    extracted.expr !== scope.offlinePrefixMatch
  ) {
    return `${scope.offlinePrefixMatch}::${extracted.expr}`;
  }
  return extracted.expr;
}
