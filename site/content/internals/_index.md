+++
title = "Inside Shoal"
description = "A maintainer's map of Shoal: crate boundaries, evaluation flow, kernel sessions, execution, protocols, persistence, security, and design invariants."
sort_by = "weight"
template = "docs/section.html"
page_template = "docs/page.html"
insert_anchor_links = "right"

[extra]
eyebrow = "Maintainer handbook"
audience = "Contributors and future maintainers"
nav_scheme = "atlas"
+++

This handbook records how Shoal fits together: the boundaries that matter, the invariants hidden
behind them, and the paths a value takes from source text to a terminal, a journal entry, or an
agent. It is written to make architectural drift visible before it becomes archaeology.
