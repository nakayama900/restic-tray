(() => {
  "use strict";

  const isJa = document.documentElement.lang.toLowerCase().startsWith("ja");

  /* ------------------------------------------------------------------------
     1. Two-state theme toggle (follows modern-web-guidance dark-mode-toggle)
     ------------------------------------------------------------------------ */
  const metaColorScheme = document.querySelector('meta[name="color-scheme"]');
  const themeBtn = document.getElementById("theme-toggle");
  const systemDarkQuery = window.matchMedia("(prefers-color-scheme: dark)");

  function getStoredScheme() {
    try {
      const value = localStorage.getItem("color-scheme");
      return value === "light" || value === "dark" ? value : null;
    } catch {
      return null;
    }
  }

  function applyColorScheme(stored) {
    if (!metaColorScheme) return;
    metaColorScheme.content = stored ?? "light dark";
  }

  if (themeBtn && metaColorScheme) {
    const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

    themeBtn.addEventListener("click", () => {
      const systemScheme = systemDarkQuery.matches ? "dark" : "light";
      const currentRendered = getStoredScheme() ?? systemScheme;
      const target = currentRendered === "dark" ? "light" : "dark";

      const switchScheme = () => {
        try {
          if (target === systemScheme) {
            localStorage.removeItem("color-scheme");
            applyColorScheme(null);
          } else {
            localStorage.setItem("color-scheme", target);
            applyColorScheme(target);
          }
        } catch {
          applyColorScheme(target);
        }
      };

      // Crossfade the whole page between themes where supported.
      if (typeof document.startViewTransition === "function" && !reduceMotion.matches) {
        document.startViewTransition(switchScheme);
      } else {
        switchScheme();
      }
    });

    window.addEventListener("storage", (event) => {
      if (event.key === "color-scheme") {
        applyColorScheme(getStoredScheme());
      }
    });
  }

  /* ------------------------------------------------------------------------
     2. Responsive Site Navigation (follows modern-web-guidance navbar)
     ------------------------------------------------------------------------ */
  const header = document.querySelector(".site-header");
  const headerInner = header?.querySelector(".header-inner");
  const siteMenu = document.getElementById("site-menu");

  if (header && headerInner && siteMenu) {
    let lastWidth = 0;

    function isPopoverOpen(el) {
      try {
        return el.matches(":popover-open") || el.classList.contains(":popover-open");
      } catch {
        return el.classList.contains(":popover-open");
      }
    }

    function updateNavLayout(force) {
      const currentWidth = headerInner.clientWidth;
      const wasWide = header.classList.contains("is-wide");

      // While collapsed, only re-measure when width has grown (prevents layout thrash)
      if (!force && !wasWide && currentWidth <= lastWidth) {
        lastWidth = currentWidth;
        return;
      }
      lastWidth = currentWidth;

      const wasOpen = !wasWide && isPopoverOpen(siteMenu);

      header.classList.add("is-measuring");
      siteMenu.removeAttribute("popover");
      header.classList.add("is-wide");

      const fitsInline = headerInner.scrollWidth <= headerInner.clientWidth + 1;

      if (!fitsInline) {
        header.classList.remove("is-wide");
        siteMenu.setAttribute("popover", "auto");
        if (wasOpen && typeof siteMenu.showPopover === "function") {
          try {
            siteMenu.showPopover();
          } catch {
            // Ignore if already open or unsupported
          }
        }
      }

      requestAnimationFrame(() => {
        header.classList.remove("is-measuring");
      });
    }

    // Close mobile popover when a same-page hash link is clicked
    siteMenu.addEventListener("click", (event) => {
      const link = event.target.closest("a");
      if (!link || header.classList.contains("is-wide")) return;
      if (typeof siteMenu.hidePopover === "function") {
        try {
          siteMenu.hidePopover();
        } catch {
          // Ignore
        }
      }
    });

    updateNavLayout(true);

    if (typeof ResizeObserver === "function") {
      const ro = new ResizeObserver(() => updateNavLayout(false));
      ro.observe(header);
      ro.observe(siteMenu);
    } else {
      window.addEventListener("resize", () => updateNavLayout(true));
    }

    // Minimal fallback toggle if browser lacks native Popover API
    if (!Object.hasOwn(HTMLElement.prototype, "popover")) {
      const menuBtn = header.querySelector(".menu-button");
      menuBtn?.addEventListener("click", () => {
        siteMenu.classList.toggle(":popover-open");
      });
    }
  }

  /* ------------------------------------------------------------------------
     3. Interactive Screenshot Tabs (WAI-ARIA Tabs pattern)
     ------------------------------------------------------------------------ */
  const tablists = document.querySelectorAll('[role="tablist"]');
  for (const tablist of tablists) {
    const tabs = Array.from(tablist.querySelectorAll('[role="tab"]'));

    function activateTab(tab, moveFocus) {
      for (const candidate of tabs) {
        const selected = candidate === tab;
        candidate.setAttribute("aria-selected", String(selected));
        candidate.tabIndex = selected ? 0 : -1;
        const panelId = candidate.getAttribute("aria-controls");
        if (panelId) {
          const panel = document.getElementById(panelId);
          if (panel) panel.hidden = !selected;
        }
      }
      if (moveFocus) tab.focus({ preventScroll: true });
      // Keep the selected tab visible when the tab strip scrolls sideways on narrow screens.
      const overflow = tablist.scrollWidth - tablist.clientWidth;
      if (overflow > 0) {
        const target = tab.offsetLeft - (tablist.clientWidth - tab.offsetWidth) / 2;
        tablist.scrollTo({ left: Math.max(0, Math.min(overflow, target)), behavior: "smooth" });
      }
    }

    const frame = tablist.parentElement?.querySelector(".showcase-frame");
    const markSwitched = () => frame?.setAttribute("data-switched", "");

    tablist.addEventListener("click", (event) => {
      const tab = event.target.closest('[role="tab"]');
      if (tab && tablist.contains(tab)) {
        markSwitched();
        activateTab(tab, false);
      }
    });

    tablist.addEventListener("keydown", (event) => {
      const current = event.target.closest('[role="tab"]');
      if (!current) return;
      const idx = tabs.indexOf(current);
      if (idx === -1) return;

      let nextIdx = null;
      if (event.key === "ArrowRight") nextIdx = (idx + 1) % tabs.length;
      else if (event.key === "ArrowLeft") nextIdx = (idx - 1 + tabs.length) % tabs.length;
      else if (event.key === "Home") nextIdx = 0;
      else if (event.key === "End") nextIdx = tabs.length - 1;

      if (nextIdx !== null) {
        event.preventDefault();
        markSwitched();
        activateTab(tabs[nextIdx], true);
      }
    });
  }

  /* ------------------------------------------------------------------------
     4. Accessible Copy-to-Clipboard for Code Blocks
     ------------------------------------------------------------------------ */
  const announcer = document.getElementById("live-announcer");
  const copyLabel = isJa ? "コピー" : "Copy";
  const copiedLabel = isJa ? "コピー完了" : "Copied";
  const announceText = isJa ? "コマンドをクリップボードにコピーしました。" : "Copied to clipboard.";

  for (const btn of document.querySelectorAll(".copy-btn")) {
    btn.addEventListener("click", async () => {
      const block = btn.closest(".code-block");
      const code = block?.querySelector("pre code");
      if (!code) return;

      const text = code.textContent ?? "";
      let ok = false;

      if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
        try {
          await navigator.clipboard.writeText(text);
          ok = true;
        } catch {
          ok = false;
        }
      }

      if (!ok) {
        try {
          const ta = document.createElement("textarea");
          ta.value = text;
          ta.setAttribute("readonly", "");
          ta.style.position = "fixed";
          ta.style.opacity = "0";
          document.body.appendChild(ta);
          ta.select();
          ok = document.execCommand("copy");
          ta.remove();
        } catch {
          ok = false;
        }
      }

      if (ok) {
        const labelEl = btn.querySelector(".copy-label");
        if (labelEl) labelEl.textContent = copiedLabel;
        btn.classList.add("is-copied");
        if (announcer) {
          announcer.textContent = "";
          requestAnimationFrame(() => {
            announcer.textContent = announceText;
          });
        }
        window.setTimeout(() => {
          if (labelEl) labelEl.textContent = copyLabel;
          btn.classList.remove("is-copied");
        }, 1800);
      }
    });
  }

  /* ------------------------------------------------------------------------
     5. Keep short inline code tokens (flags, keys, file names) whole so a line
        never breaks inside "--mcp" or "restic-tray"; spaces stay as break
        points and long paths or URLs may still wrap. Text is unchanged.
     ------------------------------------------------------------------------ */
  const ATOMIC_MAX = 24;
  for (const code of document.querySelectorAll("code")) {
    if (code.closest("pre") || code.childElementCount > 0) continue;
    const text = code.textContent ?? "";
    if (!/\s/.test(text)) {
      if (text.length <= ATOMIC_MAX) code.classList.add("is-atomic");
      continue;
    }
    const fragment = document.createDocumentFragment();
    for (const part of text.split(/(\s+)/)) {
      if (part === "") continue;
      if (/^\s+$/.test(part) || part.length > ATOMIC_MAX) {
        fragment.append(part);
      } else {
        const token = document.createElement("span");
        token.className = "is-atomic";
        token.textContent = part;
        fragment.append(token);
      }
    }
    code.replaceChildren(fragment);
  }

  /* ------------------------------------------------------------------------
     6. Header hairline once the page has scrolled
     ------------------------------------------------------------------------ */
  if (header) {
    const onScroll = () => header.classList.toggle("is-scrolled", window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
  }

  /* ------------------------------------------------------------------------
     7. Docs: heading anchor links and "On this page" scrollspy
     ------------------------------------------------------------------------ */
  const article = document.querySelector(".doc-article");
  if (article) {
    // Pointer-only affordance: hidden from assistive tech so headings keep their exact names;
    // keyboard and screen-reader users reach sections through the "On this page" list.
    for (const heading of article.querySelectorAll("h2[id], h3[id]")) {
      const link = document.createElement("a");
      link.className = "heading-anchor";
      link.href = `#${heading.id}`;
      link.tabIndex = -1;
      link.setAttribute("aria-hidden", "true");
      link.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>';
      heading.append(link);
    }

    const tocLinks = Array.from(document.querySelectorAll('.docs-toc a[href^="#"]'));
    const sections = tocLinks
      .map((link) => document.getElementById(decodeURIComponent(link.hash.slice(1))))
      .filter(Boolean);

    if (sections.length > 0 && typeof IntersectionObserver === "function") {
      const setActive = (id) => {
        for (const link of tocLinks) {
          const active = link.hash.slice(1) === id;
          link.classList.toggle("is-active", active);
          if (active) link.setAttribute("aria-current", "location");
          else link.removeAttribute("aria-current");
        }
      };
      const observer = new IntersectionObserver(
        () => {
          // Active = the last heading above the reading line (the observer box's lower edge).
          const line = window.innerHeight * 0.4 + 1;
          let current = sections[0];
          for (const section of sections) {
            if (section.getBoundingClientRect().top <= line) current = section;
          }
          setActive(current.id);
        },
        { rootMargin: "0px 0px -60% 0px", threshold: [0, 1] }
      );
      for (const section of sections) observer.observe(section);
      window.addEventListener(
        "scroll",
        () => {
          if (window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 2) {
            setActive(sections[sections.length - 1].id);
          }
        },
        { passive: true }
      );
    }
  }
})();
