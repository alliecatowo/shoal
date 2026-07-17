(() => {
  "use strict";

  const root = document.documentElement;
  const body = document.body;
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

  // Theme ------------------------------------------------------------------
  const themeButton = document.querySelector("[data-theme-toggle]");

  function setTheme(theme, persist = true) {
    root.dataset.theme = theme;
    if (persist) {
      try { localStorage.setItem("shoal-theme", theme); } catch (_) { /* private mode */ }
    }
    if (themeButton) {
      const next = theme === "dark" ? "light" : "dark";
      themeButton.setAttribute("aria-label", `Use ${next} theme`);
    }
    document.dispatchEvent(new CustomEvent("shoal:theme", { detail: { theme } }));
  }

  setTheme(root.dataset.theme || "dark", false);
  themeButton?.addEventListener("click", () => {
    setTheme(root.dataset.theme === "dark" ? "light" : "dark");
  });

  // Header and documentation drawers --------------------------------------
  const menuButton = document.querySelector("[data-mobile-menu-toggle]");
  const mobileMenu = document.querySelector("[data-mobile-menu]");
  const menuIcon = menuButton?.querySelector("[data-menu-icon]");
  const closeIcon = menuButton?.querySelector("[data-close-icon]");

  function closeMobileMenu() {
    if (!menuButton || !mobileMenu) return;
    menuButton.setAttribute("aria-expanded", "false");
    menuButton.setAttribute("aria-label", "Open navigation");
    mobileMenu.hidden = true;
    if (menuIcon) menuIcon.hidden = false;
    if (closeIcon) closeIcon.hidden = true;
  }

  menuButton?.addEventListener("click", () => {
    const willOpen = menuButton.getAttribute("aria-expanded") !== "true";
    menuButton.setAttribute("aria-expanded", String(willOpen));
    menuButton.setAttribute("aria-label", willOpen ? "Close navigation" : "Open navigation");
    if (mobileMenu) mobileMenu.hidden = !willOpen;
    if (menuIcon) menuIcon.hidden = willOpen;
    if (closeIcon) closeIcon.hidden = !willOpen;
  });

  const docsSidebar = document.querySelector("[data-docs-sidebar]");
  const sidebarOpeners = document.querySelectorAll("[data-sidebar-open]");
  const sidebarCloser = document.querySelector("[data-sidebar-close]");
  const sidebarScrim = document.querySelector("[data-sidebar-scrim]");
  const sidebarMedia = window.matchMedia("(max-width: 940px)");
  let sidebarLastFocus;

  function setSidebar(open) {
    if (!docsSidebar) return;
    const wasOpen = docsSidebar.classList.contains("is-open");
    if (open && !wasOpen) sidebarLastFocus = document.activeElement;
    docsSidebar.classList.toggle("is-open", open);
    sidebarOpeners.forEach((button) => button.setAttribute("aria-expanded", String(open)));
    if (sidebarScrim) sidebarScrim.hidden = !open;
    body.classList.toggle("sidebar-open", open);
    docsSidebar.inert = sidebarMedia.matches && !open;
    if (sidebarMedia.matches) docsSidebar.setAttribute("aria-hidden", String(!open));
    else docsSidebar.removeAttribute("aria-hidden");
    if (open) sidebarCloser?.focus();
    else if (wasOpen && sidebarLastFocus instanceof HTMLElement) sidebarLastFocus.focus();
  }

  sidebarOpeners.forEach((button) => button.addEventListener("click", () => setSidebar(true)));
  sidebarCloser?.addEventListener("click", () => setSidebar(false));
  sidebarScrim?.addEventListener("click", () => setSidebar(false));
  sidebarMedia.addEventListener("change", () => setSidebar(false));
  setSidebar(false);

  // Code blocks -------------------------------------------------------------
  document.querySelectorAll(".prose pre").forEach((pre) => {
    const language = pre.dataset.lang || pre.querySelector("code")?.dataset.lang || "";
    if (language) pre.dataset.lang = language;
    const isMermaid = language === "mermaid" || pre.querySelector("code.language-mermaid");
    if (isMermaid) return;
    const code = pre.querySelector("code") || pre;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "code-copy";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy code to clipboard");
    button.addEventListener("click", async () => {
      let copied = false;
      try {
        await navigator.clipboard.writeText(code.textContent || "");
        copied = true;
      } catch (_) {
        const selection = window.getSelection();
        const range = document.createRange();
        range.selectNodeContents(code);
        selection?.removeAllRanges();
        selection?.addRange(range);
        try { copied = document.execCommand("copy"); } catch (_) { /* selection is still useful */ }
        if (copied) selection?.removeAllRanges();
      }
      button.textContent = copied ? "Copied" : "Selected";
      button.classList.toggle("is-copied", copied);
      window.setTimeout(() => {
        button.textContent = "Copy";
        button.classList.remove("is-copied");
      }, 1600);
    });
    pre.append(button);
  });

  // Active table of contents ------------------------------------------------
  const tocLinks = [...document.querySelectorAll(".page-toc a[href^='#']")];
  if (tocLinks.length && "IntersectionObserver" in window) {
    const byId = new Map(tocLinks.map((link) => [decodeURIComponent(link.hash.slice(1)), link]));
    const headings = [...byId.keys()].map((id) => document.getElementById(id)).filter(Boolean);
    const visible = new Set();
    const observer = new IntersectionObserver((entries) => {
      entries.forEach((entry) => entry.isIntersecting ? visible.add(entry.target.id) : visible.delete(entry.target.id));
      const active = headings.find((heading) => visible.has(heading.id)) ||
        [...headings].reverse().find((heading) => heading.getBoundingClientRect().top < 150);
      tocLinks.forEach((link) => link.classList.toggle("is-active", Boolean(active && link === byId.get(active.id))));
    }, { rootMargin: "-105px 0px -72%", threshold: [0, 1] });
    headings.forEach((heading) => observer.observe(heading));
  }

  // Mermaid diagrams --------------------------------------------------------
  // Raw fences are replaced before the network request starts. Their source is
  // retained only in this WeakMap; with JavaScript disabled the original fence
  // remains as the progressive fallback.
  const mermaidSources = new WeakMap();
  const diagramStates = new WeakMap();
  const diagramShells = [];
  const diagramDialog = document.querySelector("[data-diagram-dialog]");
  const dialogTitle = diagramDialog?.querySelector("[data-diagram-dialog-title]");
  const dialogOutput = diagramDialog?.querySelector("[data-diagram-output]");
  const dialogViewport = diagramDialog?.querySelector("[data-diagram-viewport]");
  let mermaidModule;
  let mermaidLoading;
  let renderSerial = 0;
  let mermaidThemeTimer;
  let dialogState;
  let dialogLastFocus;

  const clamp = (value, min, max) => Math.min(max, Math.max(min, value));
  const copyBox = (box) => ({ x: box.x, y: box.y, width: box.width, height: box.height });

  function sourceTitle(source, fallback) {
    return source.match(/^accTitle:\s*(.+)$/m)?.[1]?.trim() || fallback || "Architecture diagram";
  }

  function nearestHeading(pre) {
    const article = pre.closest("article");
    if (!article) return "Architecture diagram";
    const headings = [...article.querySelectorAll("h2,h3")]
      .filter((heading) => heading.compareDocumentPosition(pre) & Node.DOCUMENT_POSITION_FOLLOWING);
    return headings.at(-1)?.textContent?.trim() || "Architecture diagram";
  }

  function diagramButton(action, label, text, pressed) {
    const button = document.createElement("button");
    button.type = "button";
    button.dataset.diagramAction = action;
    button.setAttribute("aria-label", label);
    if (pressed !== undefined) button.setAttribute("aria-expanded", String(pressed));
    button.textContent = text;
    return button;
  }

  function createDiagramShell(pre) {
    const source = (pre.querySelector("code") || pre).textContent || "";
    const title = sourceTitle(source, nearestHeading(pre));
    const shell = document.createElement("div");
    shell.className = "mermaid-shell";
    const toolbar = document.createElement("div");
    toolbar.className = "diagram-toolbar";
    const collapse = diagramButton("collapse", "Collapse diagram", "⌄", true);
    collapse.className = "diagram-collapse";
    const label = document.createElement("strong");
    label.className = "diagram-label";
    label.textContent = title;
    const controls = document.createElement("div");
    controls.className = "diagram-controls";
    controls.setAttribute("aria-label", "Diagram controls");
    controls.append(
      diagramButton("zoom-out", "Zoom out", "−"),
      diagramButton("fit", "Fit diagram", "Fit"),
      diagramButton("zoom-in", "Zoom in", "+"),
      diagramButton("expand", "Expand diagram", "Expand")
    );
    toolbar.append(collapse, label, controls);
    const viewport = document.createElement("div");
    viewport.className = "mermaid-viewport";
    viewport.tabIndex = 0;
    viewport.setAttribute("role", "region");
    viewport.setAttribute("aria-label", `${title}. Interactive diagram; use arrow keys to pan and plus or minus to zoom.`);
    const output = document.createElement("div");
    output.className = "mermaid-output";
    viewport.append(output);
    shell.append(toolbar, viewport);
    pre.replaceWith(shell);
    const state = { source, title, collapsed: false, zoom: 1, centerX: .5, centerY: .5, base: null, view: null };
    mermaidSources.set(shell, source);
    diagramStates.set(shell, state);
    diagramShells.push(shell);
    bindDiagramInput(shell, state, viewport, false);
    return shell;
  }

  // This loop is intentionally synchronous and precedes loadMermaid().
  [...document.querySelectorAll('.prose pre[data-lang="mermaid"]')].forEach(createDiagramShell);

  async function loadMermaid() {
    if (mermaidModule) return mermaidModule;
    if (!mermaidLoading) {
      const version = body.dataset.mermaidVersion || "11";
      mermaidLoading = import(`https://cdn.jsdelivr.net/npm/mermaid@${version}/dist/mermaid.esm.min.mjs`)
        .then((module) => (mermaidModule = module.default))
        .catch((error) => {
          mermaidLoading = undefined;
          throw error;
        });
    }
    return mermaidLoading;
  }

  function initializeMermaid(mermaid) {
    mermaid.initialize({
      startOnLoad: false,
      theme: root.dataset.theme === "light" ? "default" : "dark",
      securityLevel: "strict",
      fontFamily: getComputedStyle(root).getPropertyValue("--sans").trim(),
      flowchart: { curve: "basis", htmlLabels: true },
      sequence: { useMaxWidth: false, wrap: true }
    });
  }

  function normalizedPosition(state) {
    if (!state.base || !state.view) return { zoom: state.zoom || 1, x: state.centerX || .5, y: state.centerY || .5 };
    return {
      zoom: clamp(state.base.width / state.view.width, .75, 8),
      x: (state.view.x + state.view.width / 2 - state.base.x) / state.base.width,
      y: (state.view.y + state.view.height / 2 - state.base.y) / state.base.height
    };
  }

  function boundedBox(state, box) {
    const base = state.base;
    if (!base) return box;
    if (box.width <= base.width) box.x = clamp(box.x, base.x, base.x + base.width - box.width);
    else box.x = base.x - (box.width - base.width) / 2;
    if (box.height <= base.height) box.y = clamp(box.y, base.y, base.y + base.height - box.height);
    else box.y = base.y - (box.height - base.height) / 2;
    return box;
  }

  function applyView(host, state, box) {
    if (!state.base) return;
    const svg = host.querySelector(".mermaid-output svg");
    if (!svg) return;
    state.view = boundedBox(state, copyBox(box));
    const position = normalizedPosition(state);
    state.zoom = position.zoom;
    state.centerX = position.x;
    state.centerY = position.y;
    svg.setAttribute("viewBox", `${state.view.x} ${state.view.y} ${state.view.width} ${state.view.height}`);
  }

  function applyNormalizedView(host, state, zoom, centerX = .5, centerY = .5) {
    if (!state.base) return;
    const target = clamp(zoom, .75, 8);
    const width = state.base.width / target;
    const height = state.base.height / target;
    const centerAbsoluteX = state.base.x + clamp(centerX, 0, 1) * state.base.width;
    const centerAbsoluteY = state.base.y + clamp(centerY, 0, 1) * state.base.height;
    applyView(host, state, { x: centerAbsoluteX - width / 2, y: centerAbsoluteY - height / 2, width, height });
  }

  function fitDiagram(host, state) {
    applyNormalizedView(host, state, 1, .5, .5);
  }

  function svgPoint(svg, clientX, clientY, fallbackBox) {
    const matrix = svg.getScreenCTM();
    if (matrix && typeof DOMPoint === "function") return new DOMPoint(clientX, clientY).matrixTransform(matrix.inverse());
    const rect = svg.getBoundingClientRect();
    return {
      x: fallbackBox.x + ((clientX - rect.left) / Math.max(rect.width, 1)) * fallbackBox.width,
      y: fallbackBox.y + ((clientY - rect.top) / Math.max(rect.height, 1)) * fallbackBox.height
    };
  }

  function zoomDiagram(host, state, targetZoom, clientX, clientY) {
    if (!state.base || !state.view) return;
    const svg = host.querySelector(".mermaid-output svg");
    if (!svg) return;
    const target = clamp(targetZoom, .75, 8);
    const current = state.view;
    const width = state.base.width / target;
    const height = state.base.height / target;
    let x = current.x + (current.width - width) / 2;
    let y = current.y + (current.height - height) / 2;
    if (Number.isFinite(clientX) && Number.isFinite(clientY)) {
      const point = svgPoint(svg, clientX, clientY, current);
      const fractionX = (point.x - current.x) / current.width;
      const fractionY = (point.y - current.y) / current.height;
      x = point.x - fractionX * width;
      y = point.y - fractionY * height;
    }
    applyView(host, state, { x, y, width, height });
  }

  function panDiagram(host, state, pixelsX, pixelsY, viewport) {
    if (!state.view) return;
    const rect = viewport.getBoundingClientRect();
    const box = copyBox(state.view);
    box.x += pixelsX * box.width / Math.max(rect.width, 1);
    box.y += pixelsY * box.height / Math.max(rect.height, 1);
    applyView(host, state, box);
  }

  function toggleDiagram(shell, state) {
    state.collapsed = !state.collapsed;
    shell.classList.toggle("is-collapsed", state.collapsed);
    const button = shell.querySelector('[data-diagram-action="collapse"]');
    button?.setAttribute("aria-expanded", String(!state.collapsed));
    button?.setAttribute("aria-label", state.collapsed ? "Open diagram" : "Collapse diagram");
    if (button) button.textContent = state.collapsed ? "›" : "⌄";
  }

  function diagramAction(host, state, action) {
    if (action === "collapse") toggleDiagram(host, state);
    if (action === "zoom-out") zoomDiagram(host, state, state.zoom / 1.25);
    if (action === "zoom-in") zoomDiagram(host, state, state.zoom * 1.25);
    if (action === "fit") fitDiagram(host, state);
    if (action === "expand") openDiagramDialog(host, state);
  }

  function bindDiagramInput(host, state, viewport, allowTouch) {
    host.addEventListener("click", (event) => {
      const button = event.target.closest?.("[data-diagram-action]");
      if (button && host.contains(button)) diagramAction(host, state, button.dataset.diagramAction);
    });

    viewport.addEventListener("wheel", (event) => {
      if (!event.ctrlKey && !event.metaKey) return;
      event.preventDefault();
      zoomDiagram(host, state, state.zoom * Math.exp(-event.deltaY * .002), event.clientX, event.clientY);
    }, { passive: false });

    viewport.addEventListener("keydown", (event) => {
      const distance = event.shiftKey ? 80 : 24;
      if (event.key === "ArrowLeft") panDiagram(host, state, -distance, 0, viewport);
      else if (event.key === "ArrowRight") panDiagram(host, state, distance, 0, viewport);
      else if (event.key === "ArrowUp") panDiagram(host, state, 0, -distance, viewport);
      else if (event.key === "ArrowDown") panDiagram(host, state, 0, distance, viewport);
      else if (event.key === "+" || event.key === "=") zoomDiagram(host, state, state.zoom * 1.25);
      else if (event.key === "-" || event.key === "_") zoomDiagram(host, state, state.zoom / 1.25);
      else if (event.key === "0" || event.key.toLowerCase() === "f") fitDiagram(host, state);
      else return;
      event.preventDefault();
    });

    const pointers = new Map();
    let pinch;
    viewport.addEventListener("pointerdown", (event) => {
      if (event.button !== 0 || (!allowTouch && event.pointerType === "touch")) return;
      pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
      viewport.setPointerCapture?.(event.pointerId);
      viewport.classList.add("is-dragging");
      if (pointers.size === 2) {
        const [a, b] = [...pointers.values()];
        pinch = { distance: Math.hypot(a.x - b.x, a.y - b.y), zoom: state.zoom };
      }
      event.preventDefault();
    });
    viewport.addEventListener("pointermove", (event) => {
      const previous = pointers.get(event.pointerId);
      if (!previous) return;
      pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
      if (pointers.size === 2 && pinch) {
        const [a, b] = [...pointers.values()];
        const distance = Math.hypot(a.x - b.x, a.y - b.y);
        zoomDiagram(host, state, pinch.zoom * distance / Math.max(pinch.distance, 1), (a.x + b.x) / 2, (a.y + b.y) / 2);
      } else if (pointers.size === 1) {
        panDiagram(host, state, previous.x - event.clientX, previous.y - event.clientY, viewport);
      }
      event.preventDefault();
    });
    const release = (event) => {
      pointers.delete(event.pointerId);
      if (pointers.size < 2) pinch = undefined;
      if (!pointers.size) viewport.classList.remove("is-dragging");
    };
    viewport.addEventListener("pointerup", release);
    viewport.addEventListener("pointercancel", release);
  }

  function setDiagramUnavailable(host, state) {
    const output = host.querySelector(".mermaid-output");
    if (!output) return;
    output.replaceChildren();
    const error = document.createElement("div");
    error.className = "mermaid-error";
    const message = document.createElement("p");
    message.textContent = "Diagram unavailable";
    const retry = document.createElement("button");
    retry.type = "button";
    retry.textContent = "Retry";
    retry.addEventListener("click", () => renderDiagram(host, state));
    error.append(message, retry);
    output.append(error);
  }

  async function renderDiagram(host, state) {
    const output = host.querySelector(".mermaid-output");
    if (!output || !state?.source) return;
    const previous = normalizedPosition(state);
    output.setAttribute("aria-busy", "true");
    try {
      const mermaid = await loadMermaid();
      initializeMermaid(mermaid);
      const id = `shoal-diagram-${++renderSerial}`;
      const { svg, bindFunctions } = await mermaid.render(id, state.source);
      output.innerHTML = svg;
      bindFunctions?.(output);
      const rendered = output.querySelector("svg");
      if (!rendered) throw new Error("Mermaid returned no SVG");
      rendered.removeAttribute("width");
      rendered.removeAttribute("height");
      rendered.setAttribute("preserveAspectRatio", "xMidYMid meet");
      if (!rendered.hasAttribute("aria-labelledby") && !rendered.hasAttribute("aria-label")) {
        rendered.setAttribute("role", "img");
        rendered.setAttribute("aria-label", state.title);
      }
      const viewBox = rendered.viewBox?.baseVal;
      if (!viewBox?.width || !viewBox?.height) throw new Error("Mermaid SVG has no viewBox");
      state.base = copyBox(viewBox);
      state.view = copyBox(viewBox);
      host.classList.toggle("is-wide-diagram", viewBox.width / viewBox.height > 2.45);
      applyNormalizedView(host, state, previous.zoom, previous.x, previous.y);
      output.removeAttribute("aria-busy");
    } catch (_) {
      output.removeAttribute("aria-busy");
      setDiagramUnavailable(host, state);
    }
  }

  async function renderAllDiagrams() {
    for (const shell of diagramShells) await renderDiagram(shell, diagramStates.get(shell));
  }

  function openDiagramDialog(shell, sourceState) {
    if (!diagramDialog || !dialogOutput || !dialogViewport) return;
    dialogLastFocus = document.activeElement;
    const position = normalizedPosition(sourceState);
    dialogState = {
      source: mermaidSources.get(shell), title: sourceState.title, collapsed: false,
      zoom: position.zoom, centerX: position.x, centerY: position.y, base: null, view: null
    };
    if (dialogTitle) dialogTitle.textContent = sourceState.title;
    diagramDialog.setAttribute("aria-label", sourceState.title);
    diagramDialog.showModal();
    renderDiagram(diagramDialog, dialogState).then(() => dialogViewport.focus());
  }

  function closeDiagramDialog() {
    if (!diagramDialog?.open) return;
    diagramDialog.close();
    dialogOutput?.replaceChildren();
    dialogState = undefined;
    if (dialogLastFocus instanceof HTMLElement) dialogLastFocus.focus();
  }

  if (diagramDialog && dialogViewport) {
    // Dialog state is looked up at event time because the reusable dialog gets
    // a freshly rendered, uniquely identified SVG on every open.
    const proxyState = new Proxy({}, {
      get: (_, key) => dialogState?.[key],
      set: (_, key, value) => { if (dialogState) dialogState[key] = value; return true; }
    });
    bindDiagramInput(diagramDialog, proxyState, dialogViewport, true);
    diagramDialog.querySelector("[data-diagram-close]")?.addEventListener("click", closeDiagramDialog);
    diagramDialog.addEventListener("click", (event) => { if (event.target === diagramDialog) closeDiagramDialog(); });
    diagramDialog.addEventListener("cancel", (event) => { event.preventDefault(); closeDiagramDialog(); });
  }

  if (diagramShells.length) renderAllDiagrams();
  document.addEventListener("shoal:theme", () => {
    if (!diagramShells.length) return;
    window.clearTimeout(mermaidThemeTimer);
    mermaidThemeTimer = window.setTimeout(async () => {
      await renderAllDiagrams();
      if (diagramDialog?.open && dialogState) await renderDiagram(diagramDialog, dialogState);
    }, reducedMotion.matches ? 0 : 100);
  });

  let printDiagramState;
  window.addEventListener("beforeprint", () => {
    printDiagramState = diagramShells.map((shell) => ({ shell, state: diagramStates.get(shell), position: normalizedPosition(diagramStates.get(shell)) }));
    printDiagramState.forEach(({ shell, state }) => {
      shell.classList.remove("is-collapsed");
      fitDiagram(shell, state);
    });
  });
  window.addEventListener("afterprint", () => {
    printDiagramState?.forEach(({ shell, state, position }) => {
      shell.classList.toggle("is-collapsed", state.collapsed);
      applyNormalizedView(shell, state, position.zoom, position.x, position.y);
    });
    printDiagramState = undefined;
  });

  // Search ------------------------------------------------------------------
  const searchDialog = document.querySelector("[data-search-dialog]");
  const searchInput = document.querySelector("[data-search-input]");
  const searchResults = document.querySelector("[data-search-results]");
  const searchEmpty = document.querySelector("[data-search-empty]");
  let searchDocuments;
  let selectedResult = -1;
  let lastFocused;

  const cleanText = (value = "") => String(value)
    .replace(/<[^>]+>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  const fold = (value = "") => cleanText(value).toLocaleLowerCase().normalize("NFKD").replace(/[\u0300-\u036f]/g, "");

  async function getSearchDocuments() {
    if (searchDocuments) return searchDocuments;
    const response = await fetch(body.dataset.searchIndex, { credentials: "same-origin" });
    if (!response.ok) throw new Error("Search index unavailable");
    const payload = await response.json();
    let documents = Array.isArray(payload) ? payload : payload.docs;
    if (!Array.isArray(documents) && payload.documentStore?.docs) documents = Object.values(payload.documentStore.docs);
    searchDocuments = (documents || []).map((doc) => ({
      title: cleanText(doc.title || "Untitled"),
      description: cleanText(doc.description || doc.body || doc.content || ""),
      body: fold(doc.body || doc.content || doc.description || ""),
      path: doc.path || doc.url || doc.ref || "/",
      url: doc.url || doc.path || doc.ref || "/"
    }));
    return searchDocuments;
  }

  function rankDocument(doc, terms) {
    const title = fold(doc.title);
    const description = fold(doc.description);
    const path = fold(doc.path);
    let score = 0;
    for (const term of terms) {
      if (!title.includes(term) && !description.includes(term) && !doc.body.includes(term) && !path.includes(term)) return 0;
      if (title === term) score += 90;
      else if (title.startsWith(term)) score += 55;
      else if (title.includes(term)) score += 35;
      if (description.includes(term)) score += 12;
      if (path.includes(term)) score += 8;
      if (doc.body.includes(term)) score += 2;
    }
    return score;
  }

  function renderResults(documents, query) {
    if (!searchResults || !searchEmpty) return;
    searchResults.replaceChildren();
    selectedResult = -1;
    searchInput?.removeAttribute("aria-activedescendant");
    const terms = fold(query).split(" ").filter((term) => term.length > 1);
    if (!terms.length) {
      searchEmpty.hidden = true;
      return;
    }
    const ranked = documents.map((doc) => ({ doc, score: rankDocument(doc, terms) }))
      .filter(({ score }) => score > 0).sort((a, b) => b.score - a.score).slice(0, 12);
    searchEmpty.hidden = ranked.length > 0;

    for (const [index, { doc }] of ranked.entries()) {
      const link = document.createElement("a");
      link.className = "search-result";
      link.id = `search-result-${index}`;
      link.href = doc.url;
      link.setAttribute("role", "option");
      const copy = document.createElement("span");
      copy.className = "search-result-copy";
      const title = document.createElement("strong");
      title.textContent = doc.title;
      const description = document.createElement("p");
      description.textContent = doc.description.slice(0, 210);
      const kind = document.createElement("small");
      kind.textContent = doc.url.includes("/internals/") ? "Architecture" : "Manual";
      copy.append(title, description);
      link.append(copy, kind);
      searchResults.append(link);
    }
  }

  function moveSearchSelection(delta) {
    const results = [...document.querySelectorAll(".search-result")];
    if (!results.length) return;
    selectedResult = (selectedResult + delta + results.length) % results.length;
    results.forEach((result, index) => {
      result.classList.toggle("is-selected", index === selectedResult);
      result.setAttribute("aria-selected", String(index === selectedResult));
    });
    searchInput?.setAttribute("aria-activedescendant", results[selectedResult].id);
    results[selectedResult].scrollIntoView({ block: "nearest" });
  }

  async function openSearch() {
    if (!searchDialog) return;
    lastFocused = document.activeElement;
    closeMobileMenu();
    if (!searchDialog.open) searchDialog.showModal();
    window.setTimeout(() => searchInput?.focus(), 0);
    try { await getSearchDocuments(); }
    catch (_) {
      if (searchEmpty) {
        searchEmpty.textContent = "Search could not load. Check your connection and try again.";
        searchEmpty.hidden = false;
      }
    }
  }

  function closeSearch() {
    searchDialog?.close();
    if (lastFocused instanceof HTMLElement) lastFocused.focus();
  }

  document.querySelectorAll("[data-search-open]").forEach((button) => button.addEventListener("click", openSearch));
  document.querySelector("[data-search-close]")?.addEventListener("click", closeSearch);
  searchDialog?.addEventListener("click", (event) => {
    if (event.target === searchDialog) closeSearch();
  });
  searchInput?.addEventListener("input", async () => {
    try { renderResults(await getSearchDocuments(), searchInput.value); } catch (_) { /* reported by open */ }
  });
  searchInput?.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") { event.preventDefault(); moveSearchSelection(1); }
    if (event.key === "ArrowUp") { event.preventDefault(); moveSearchSelection(-1); }
    if (event.key === "Enter" && selectedResult >= 0) {
      event.preventDefault();
      document.querySelectorAll(".search-result")[selectedResult]?.click();
    }
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Tab" && docsSidebar?.classList.contains("is-open")) {
      const focusable = [...docsSidebar.querySelectorAll('a[href], button:not([disabled]), summary, [tabindex]:not([tabindex="-1"])')]
        .filter((item) => {
          if (!(item instanceof HTMLElement) || item.getClientRects().length === 0) return false;
          let node = item;
          while (node.parentElement && node.parentElement !== docsSidebar) {
            const parent = node.parentElement;
            if (parent instanceof HTMLDetailsElement && !parent.open) {
              const summary = parent.querySelector(":scope > summary");
              if (item !== summary && !summary?.contains(item)) return false;
            }
            node = parent;
          }
          return true;
        });
      const first = focusable[0];
      const last = focusable.at(-1);
      if (first && last) {
        if (!docsSidebar.contains(document.activeElement)) {
          event.preventDefault();
          first.focus();
        } else if (event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }
    }
    const typing = event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement || event.target?.isContentEditable;
    if ((event.key === "/" && !typing) || (event.key.toLowerCase() === "k" && (event.metaKey || event.ctrlKey))) {
      event.preventDefault();
      openSearch();
    }
    if (event.key === "Escape") {
      if (searchDialog?.open) closeSearch();
      setSidebar(false);
      closeMobileMenu();
    }
  });
})();
