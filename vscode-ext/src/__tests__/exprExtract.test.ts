import { strict as assert } from "node:assert";
import test from "node:test";
import {
  buildScopedCandidates,
  extractLeftChainAtOffset,
  splitQualifiedSymbolName,
} from "../exprExtractCore";

function extractAtMarker(snippet: string): ReturnType<typeof extractLeftChainAtOffset> {
  const marker = "|";
  const idx = snippet.indexOf(marker);
  assert.notEqual(idx, -1, `missing cursor marker | in ${snippet}`);
  const clean = snippet.slice(0, idx) + snippet.slice(idx + 1);
  return extractLeftChainAtOffset(clean, idx);
}

test("extracts left chain along ., ->, ::, and [i] without expanding right", () => {
  assert.equal(extractAtMarker("ConceptArm::left|Arm.tick();")?.expr, "ConceptArm::leftArm");
  assert.equal(extractAtMarker("ConceptArm::leftArm|;")?.expr, "ConceptArm::leftArm");
  assert.equal(extractAtMarker("g_ctrl.pi|d[2].gain = 1;")?.expr, "g_ctrl.pid");
  assert.equal(extractAtMarker("g_ctrl.pid[2].ga|in = 1;")?.expr, "g_ctrl.pid[2].gain");
  assert.equal(extractAtMarker("g_ptr->next->va|l;")?.expr, "g_ptr->next->val");
});

test("Q1: cursor inside [...] snaps outward to outermost enclosing subscript", () => {
  assert.equal(extractAtMarker("g_arr[|2] = 1;")?.expr, "g_arr[2]");
  assert.equal(extractAtMarker("g_arr[i|dx] = 1;")?.expr, "g_arr[idx]");
  assert.equal(extractAtMarker("g_arr[idx]| = 1;")?.expr, "g_arr[idx]");
  assert.equal(extractAtMarker("g_arr|[idx] = 1;")?.expr, "g_arr");
  assert.equal(extractAtMarker("g_arr[|idx] = 1;")?.expr, "g_arr[idx]");
  assert.equal(extractAtMarker("a[b[|i]].val = 1;")?.expr, "a[b[i]]");
  assert.equal(extractAtMarker("a[b[i]].va|l = 1;")?.expr, "a[b[i]].val");
  assert.equal(extractAtMarker("mat[1][|2] = 1;")?.expr, "mat[1][2]");
});

test("supports templates on ::, multiline chains, (*ptr).x, and leading ::", () => {
  assert.equal(
    extractAtMarker("DamiaoMotor<os::Can<1>, true>::s_cou|nt++;")?.expr,
    "DamiaoMotor<os::Can<1>, true>::s_count",
  );
  assert.equal(
    extractAtMarker("ConceptArm::leftArm\n  ->frame_\n  .joint|State;")?.expr,
    "ConceptArm::leftArm->frame_.jointState",
  );
  assert.equal(extractAtMarker("(*g_state_ptr).mo|de;")?.expr, "(*g_state_ptr).mode");

  const global = extractAtMarker("::uw|Tick;");
  assert.equal(global?.expr, "::uwTick");
  assert.equal(global?.explicitGlobal, true);
  assert.equal(global?.rootIdent, "uwTick");
});

test("respects unary boundaries and ignores function call chains / attributes / keywords", () => {
  assert.equal(extractAtMarker("*g_ptr->ne|xt;")?.expr, "g_ptr->next");
  assert.equal(extractAtMarker("&g_ctrl.ga|in;")?.expr, "g_ctrl.gain");
  assert.equal(extractAtMarker("ConceptArm::leftArm.getJointState().ang|le;"), undefined);
  assert.equal(extractAtMarker("[[maybe|_unused]] int x = 0;"), undefined);
  assert.equal(extractAtMarker("if| (x > 0) {}"), undefined);
});

test("splits DocumentSymbol names and generates innermost-to-global candidates", () => {
  assert.deepEqual(
    splitQualifiedSymbolName("ConceptArm::LeftArm::tick(unsigned int)"),
    ["ConceptArm", "LeftArm", "tick"],
  );
  assert.deepEqual(
    splitQualifiedSymbolName("foc::(anonymous namespace)::LowPass<float>::update()"),
    ["foc", "LowPass<float>", "update"],
  );
  assert.deepEqual(
    buildScopedCandidates("pid[0].kp", false, ["foc", "axis0", "Controller"]),
    [
      "foc::axis0::Controller::pid[0].kp",
      "foc::axis0::pid[0].kp",
      "foc::pid[0].kp",
      "pid[0].kp",
    ],
  );
  assert.deepEqual(
    buildScopedCandidates("ConceptArm::leftArm", false, ["ConceptArm", "LeftArm"]),
    ["ConceptArm::LeftArm::ConceptArm::leftArm", "ConceptArm::leftArm"],
  );
  assert.deepEqual(buildScopedCandidates("::uwTick", true, ["foc", "axis0"]), ["::uwTick"]);
});
