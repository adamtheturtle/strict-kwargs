Bundle the ty inference engine instead of launching a separate server.
Reuse analysis workers across phases.
Apply keyword insertions in one pass to avoid quadratic work when fixing many calls in a large file.
Index call sites and edits when applying ty-resolved fixes instead of repeatedly scanning the AST and previous edits.
Reduce contention in cached definition lookups when checking files in parallel.
