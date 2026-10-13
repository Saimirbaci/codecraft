// JXA: open each lazily-filled submenu of the running app so it populates, then read it.
// argv[0] = process name, argv[1] = JSON array of paths (each an array of titles).
function run(argv) {
  const procName = argv[0];
  const paths = JSON.parse(argv[1]);
  const se = Application("System Events");
  const front = se.processes.whose({ frontmost: true })[0].name();
  const proc = se.processes.byName(procName);
  proc.frontmost = true;
  delay(0.6);
  const out = [];
  function attr(el, name) {
    try { const v = el.attributes.byName(name).value(); return v === undefined ? null : v; }
    catch (e) { return null; }
  }
  function walk(menu, path, depth) {
    let items;
    try { items = menu.menuItems(); } catch (e) { return; }
    for (const it of items) {
      let title = null;
      try { title = it.title(); } catch (e) {}
      if (!title) { out.push(JSON.stringify({ path: path, separator: true })); continue; }
      let sub = null;
      try { if (it.menus.length > 0) sub = it.menus[0]; } catch (e) {}
      out.push(JSON.stringify({
        path: path, title: title,
        cmdChar: attr(it, "AXMenuItemCmdChar"), cmdGlyph: attr(it, "AXMenuItemCmdGlyph"),
        cmdVirtualKey: attr(it, "AXMenuItemCmdVirtualKey"), cmdModifiers: attr(it, "AXMenuItemCmdModifiers"),
        enabled: attr(it, "AXEnabled"), hasSubmenu: !!sub,
      }));
      if (sub && depth < 2) {
        try { it.click(); delay(0.4); } catch (e) {}
        walk(sub, path.concat([title]), depth + 1);
      }
    }
  }
  for (const p of paths) {
    try {
      const top = proc.menuBars[0].menuBarItems.byName(p[0]);
      top.click(); delay(0.5);
      let menu = top.menus[0];
      let item = null;
      for (let i = 1; i < p.length; i++) {
        item = menu.menuItems.byName(p[i]);
        item.click(); delay(0.7);
        menu = item.menus[0];
      }
      walk(menu, p, 0);
    } catch (e) {
      out.push(JSON.stringify({ path: p, error: String(e) }));
    }
    se.keyCode(53); delay(0.15); se.keyCode(53); delay(0.15); se.keyCode(53); delay(0.3);
  }
  try { se.processes.byName(front).frontmost = true; } catch (e) {}
  return out.join("\n");
}
