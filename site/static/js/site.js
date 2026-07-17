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
  const mermaidSources = new Map();
  let mermaidModule;
  let mermaidLoading;

  async function loadMermaid() {
    if (mermaidModule) return mermaidModule;
    if (!mermaidLoading) {
      const version = body.dataset.mermaidVersion || "11";
      mermaidLoading = import(`https://cdn.jsdelivr.net/npm/mermaid@${version}/dist/mermaid.esm.min.mjs`)
        .then((module) => (mermaidModule = module.default));
    }
    return mermaidLoading;
  }

  async function drawMermaid() {
    const untouched = [...document.querySelectorAll('.prose pre[data-lang="mermaid"]')]
      .filter((pre) => !pre.closest(".mermaid-source"));
    const existing = [...document.querySelectorAll(".mermaid-shell")];
    if (!untouched.length && !existing.length) return;

    let mermaid;
    try { mermaid = await loadMermaid(); } catch (_) { return; } // Source remains visible.

    for (const pre of untouched) {
      const source = (pre.querySelector("code") || pre).textContent || "";
      const shell = document.createElement("div");
      shell.className = "mermaid-shell";
      shell.innerHTML = '<div class="mermaid-output" role="img"></div><details class="mermaid-source"><summary>Diagram source</summary></details>';
      pre.before(shell);
      shell.querySelector(".mermaid-source")?.append(pre);
      mermaidSources.set(shell, source);
    }

    mermaid.initialize({
      startOnLoad: false,
      theme: root.dataset.theme === "light" ? "default" : "dark",
      securityLevel: "strict",
      fontFamily: getComputedStyle(root).getPropertyValue("--sans").trim(),
      flowchart: { curve: "basis", htmlLabels: true },
      sequence: { useMaxWidth: true, wrap: true }
    });

    for (const [index, shell] of [...document.querySelectorAll(".mermaid-shell")].entries()) {
      const source = mermaidSources.get(shell) || shell.querySelector("pre")?.textContent || "";
      mermaidSources.set(shell, source);
      const output = shell.querySelector(".mermaid-output");
      if (!output) continue;
      try {
        const id = `shoal-diagram-${Date.now()}-${index}`;
        const { svg, bindFunctions } = await mermaid.render(id, source);
        output.innerHTML = svg;
        output.setAttribute("aria-label", shell.closest("section")?.querySelector("h2,h3")?.textContent || "Architecture diagram");
        bindFunctions?.(output);
      } catch (_) {
        output.innerHTML = '<p class="mermaid-error">This diagram could not be rendered. Its source is available below.</p>';
      }
    }
  }

  drawMermaid();
  let mermaidThemeTimer;
  document.addEventListener("shoal:theme", () => {
    if (!document.querySelector(".mermaid-shell")) return;
    window.clearTimeout(mermaidThemeTimer);
    mermaidThemeTimer = window.setTimeout(drawMermaid, reducedMotion.matches ? 0 : 100);
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
      kind.textContent = doc.url.includes("/internals/") ? "internal" : "guide";
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
