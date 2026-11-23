import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(new URL("../src/ui.js", import.meta.url), "utf8");
const createRow = source.slice(
  source.indexOf("function createClipRow("),
  source.indexOf("function renderClipboard("),
);

function setup() {
  const elements = [];
  const calls = [];
  const notices = [];
  const context = vm.createContext({
    document: {
      createElement(tag) {
        const element = {
          tag,
          append() {},
          setAttribute() {},
          focus() { this.focused = true; },
        };
        elements.push(element);
        return element;
      },
    },
    formatTime: () => "",
    run: task => task(),
    api: async (command, args) => {
      calls.push({ command, ...args });
      return { ring: [] };
    },
    renderClipboard() {},
    setNotice: (...args) => notices.push(args),
  });
  vm.runInContext(`${createRow}; createClipRow({id: "entry", content: "文本", pinned: true})`, context);
  return {
    code: elements.find(element => element.tag === "input"),
    save: elements.find(element => element.textContent === "保存简码"),
    calls,
    notices,
  };
}

for (const [input, expected] of [[" KSB ", "ksb"], ["", null], ["  ", null], ["abcdefghijklmnop", "abcdefghijklmnop"]]) {
  test(`收藏简码规范化并保存 ${JSON.stringify(input)}`, async () => {
    const { code, save, calls } = setup();
    code.value = input;
    await save.onclick();
    assert.equal(calls.length, 1);
    assert.equal(calls[0].command, "clipboard-pin");
    assert.equal(calls[0].id, "entry");
    assert.equal(calls[0].shortcut_code, expected);
    assert.equal(code.value, expected || "");
  });
}

for (const input of ["ks1", "ks-b", "ks b", "abcdefghijklmnopq", "中文", "\u212A"]) {
  test(`收藏简码无效时阻止请求 ${JSON.stringify(input)}`, async () => {
    const { code, save, calls, notices } = setup();
    code.value = input;
    await save.onclick();
    assert.equal(calls.length, 0);
    assert.equal(notices[0][1], "error");
    assert.match(notices[0][0], /1–16.*英文字母/);
    assert.equal(code.focused, true);
  });
}
