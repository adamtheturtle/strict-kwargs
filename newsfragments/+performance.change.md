Require ty 0.0.103 or newer.
Reuse analysis workers across phases.
Apply keyword insertions in one pass to avoid quadratic work when fixing many calls in a large file.
Index call sites and edits when applying ty-resolved fixes instead of repeatedly scanning the AST and previous edits.
