"""Decode the Accessibility menu dump into shortcut strings (macOS keymap)."""
import json
import sys

GLYPHS = {
    0x02: "Tab", 0x03: "Shift+Tab", 0x04: "Enter", 0x09: "Space", 0x0A: "Delete",
    0x0B: "Return", 0x17: "Backspace", 0x1B: "Escape", 0x62: "PageUp", 0x64: "Left",
    0x65: "Right", 0x66: "Home", 0x68: "Up", 0x69: "End", 0x6A: "Down", 0x6B: "PageDown",
}
for i in range(12):
    GLYPHS[0x6F + i] = f"F{i + 1}"
VKEYS = {
    122: "F1", 120: "F2", 99: "F3", 118: "F4", 96: "F5", 97: "F6", 98: "F7", 100: "F8",
    101: "F9", 109: "F10", 103: "F11", 111: "F12", 51: "Backspace", 117: "Delete",
    36: "Return", 53: "Escape", 48: "Tab", 123: "Left", 124: "Right", 125: "Down", 126: "Up",
    115: "Home", 119: "End", 116: "PageUp", 121: "PageDown", 49: "Space",
}


def shortcut(rec):
    key = rec.get("cmdChar")
    if not key and rec.get("cmdGlyph"):
        key = GLYPHS.get(rec["cmdGlyph"], f"glyph{rec['cmdGlyph']:#x}")
    if not key and rec.get("cmdVirtualKey") is not None:
        key = VKEYS.get(rec["cmdVirtualKey"], f"vk{rec['cmdVirtualKey']}")
    if not key:
        return ""
    fk = {0xF700: "Up", 0xF701: "Down", 0xF702: "Left", 0xF703: "Right", 0xF727: "Insert",
          0xF728: "Delete", 0xF729: "Home", 0xF72B: "End", 0xF72C: "PageUp", 0xF72D: "PageDown"}
    for i in range(35):
        fk[0xF704 + i] = f"F{i + 1}"
    named = {"\x7f": "Backspace", "\x08": "Backspace", "\t": "Tab", "\n": "Return", "\r": "Return",
             " ": "Space", "\x1b": "Escape", "\x03": "Enter"}
    if len(key) == 1 and ord(key) in fk:
        key = fk[ord(key)]
    elif key in named:
        key = named[key]
    mods = rec.get("cmdModifiers") or 0
    parts = []
    if mods & 4:
        parts.append("Ctrl")
    if mods & 2:
        parts.append("Alt")
    if mods & 1:
        parts.append("Shift")
    if not mods & 8:
        parts.append("Cmd")
    return "+".join(parts + [key.upper() if len(key) == 1 else key])


def main():
    rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
    mode = sys.argv[2] if len(sys.argv) > 2 else "outline"
    if mode == "json":
        out = []
        for r in rows:
            if r.get("separator"):
                out.append({"path": r["path"], "separator": True})
                continue
            out.append({"path": r["path"], "title": r["title"], "shortcut": shortcut(r),
                        "submenu": r["hasSubmenu"]})
        json.dump(out, sys.stdout, indent=1, ensure_ascii=False)
        return
    for r in rows:
        depth = len(r["path"]) - 1
        if r.get("separator"):
            print("  " * depth + "  ----")
            continue
        sc = shortcut(r)
        sub = " ▸" if r["hasSubmenu"] else ""
        top = r["path"][0] if depth == 0 else ""
        prefix = f"[{top}] " if top else ""
        print("  " * depth + prefix + r["title"] + sub + (f"   ({sc})" if sc else ""))


main()
