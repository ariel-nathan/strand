;; ticker — logs on a 50ms cadence for the duration of the demo.
;; Its ticks must appear *during* ping's 300ms sleep. That interleaving,
;; on a runtime pinned to ONE worker thread, is the M0 proof.
(module
  (import "strand" "log"      (func $log   (param i32 i32)))
  (import "strand" "sleep_ms" (func $sleep (param i64)))

  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 1024))

  (data (i32.const 64) "tick")

  (func (export "strand_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))

  (func (export "strand_main")
    (local $i i32)
    (block $done
      (loop $l
        (br_if $done (i32.ge_s (local.get $i) (i32.const 8)))
        (call $log (i32.const 64) (i32.const 4))
        (call $sleep (i64.const 50))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l))))
)
