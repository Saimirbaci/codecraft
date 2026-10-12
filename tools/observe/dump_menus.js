// JXA: walk the running app's screen menu bar through Accessibility and print
// one JSON line per item: path, title, shortcut char/glyph/virtual key, modifiers.
// Read-only observation of the visible UI.
function run(argv) {
  const procName = argv[0] || "rustrover";
  const maxDepth = parseInt(argv[1] || "4", 10);
  const se = Application("System Events");
  const proc = se.processes.byName(procName);
  const bar = proc.menuBars[0];
  const out = [];

  function attr(el, name) {
    try { const v = el.attributes.byName(name).value(); return v === undefined ? null : v; }
    catch (e) { return null; }
  }

  function walkMenu(menu, path, depth) {
    let items;
    try { items = menu.menuItems(); } catch (e) { return; }
    for (const it of items) {
      let title = null;
      try { title = it.title(); } catch (e) {}
      if (!title) { out.push(JSON.stringify({ path: path, separator: true })); continue; }
      const rec = {
        path: path,
        title: title,
        cmdChar: attr(it, "AXMenuItemCmdChar"),
        cmdGlyph: attr(it, "AXMenuItemCmdGlyph"),
        cmdVirtualKey: attr(it, "AXMenuItemCmdVirtualKey"),
        cmdModifiers: attr(it, "AXMenuItemCmdModifiers"),
        enabled: attr(it, "AXEnabled"),
        hasSubmenu: false,
      };
      let sub = null;
      try { if (it.menus.length > 0) sub = it.menus[0]; } catch (e) {}
      if (sub) rec.hasSubmenu = true;
      out.push(JSON.stringify(rec));
      if (sub && depth < maxDepth) walkMenu(sub, path.concat([title]), depth + 1);
    }
  }

  const tops = bar.menuBarItems();
  for (const top of tops) {
    let name = null;
    try { name = top.name(); } catch (e) {}
    if (!name || name === "Apple") continue;
    let menu = null;
    try { menu = top.menus[0]; } catch (e) {}
    if (menu) walkMenu(menu, [name], 1);
  }
  return out.join("\n");
}
