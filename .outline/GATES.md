# Gates: oldtest campaign — window/screen/process builtin surface (leaf C)

Scope: systemlist (process capture through ox-uv job channels), winwidth/winheight/wincmd/win_getid (window model + :resize/:wincmd ex commands), screenattr/screenchar family (screen-state query over the compositor grid), matchstrlist, getbufvar, fullcommand, eventhandler, echohl, and the measured E739/E121/E114 dict/variable semantics gaps on test_functions.vim.

Baseline (measured, ddeef85): test_functions.vim — 110 executed / 29 passed / 79 failed / 2 skipped.

- [x] G1: no E117 'not implemented' for the scoped builtins in a fresh harness run
  CHECK: cd .references/neovim/test/old/testdir && rm -f messages && timeout 600 /home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim >/dev/null 2>&1; grep -ao 'not implemented: \(systemlist\|winwidth\|wincmd\|win_getid\|screenattr\|resize\|matchstrlist\|getbufvar\|fullcommand\|eventhandler\|echohl\)' messages | wc -l
  EXPECT: 0
  EVIDENCE: fresh harness: 0 scoped E117 not-implemented matches

- [x] G2: passed count improves over the 29-passed baseline
  CHECK: cd .references/neovim/test/old/testdir && E=$(grep -aoE '^Executed [0-9]+ tests?' messages | grep -oE '[0-9]+' | tail -1); F=$(grep -aoE '^[0-9]+ FAILED:' messages | grep -oE '[0-9]+' | tail -1); S=$(grep -ac '^SKIPPED' messages); P=$((E-F-S)); test "$P" -gt 29 && echo "IMPROVED: $P passed"
  EXPECT: IMPROVED
  EVIDENCE: 110 executed / 34 passed / 74 failed / 2 skipped (baseline 29 passed)

- [x] G3: crate gate green (ox-editor + ox-eval)
  CHECK: cargo nextest run -p ox-editor -p ox-eval >/dev/null 2>&1 && echo ALLGREEN
  EXPECT: ALLGREEN
  EVIDENCE: cargo nextest: 954 passed, 0 skipped

- [x] G4: systemlist round-trips through a real process (unit test: systemlist('printf hello') == ['hello'] shape, list-arg form, shell-error dict in v:shell_error)
  CHECK: cargo nextest run -p ox-editor -E 'test(/systemlist|system/)' 2>&1 | tail -3
  EXPECT: /[1-9][0-9]* passed/
  EVIDENCE: system/systemlist filter: 4 passed, 565 skipped

- [x] G5: no per-test process-level abort on the unfiltered run
  CHECK: cd .references/neovim/test/old/testdir && timeout 600 /home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim 2>&1 | grep -c 'Ex command failed' || true
  EXPECT: 0
  EVIDENCE: fresh unfiltered run contains 0 process-level Ex command failed aborts

- [ ] G6: all commits for this leaf pushed to origin/main
  CHECK: git log origin/main..HEAD --oneline | wc -l
  EXPECT: 0
  EVIDENCE: pending
