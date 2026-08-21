;; pong — replies to whatever it is sent. No main-loop of its own.
(module
  (import "strand" "log"  (func $log  (param i32 i32)))
  (import "strand" "send" (func $send (param i32 i32 i32)))

  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 1024))

  (data (i32.const 64)  "pong: ready")
  (data (i32.const 192) "PONG")

  (func (export "strand_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))

  (func (export "strand_main")
    (call $log (i32.const 64) (i32.const 11)))

  (func (export "strand_on_message") (param $ptr i32) (param $len i32)
    (call $log (local.get $ptr) (local.get $len))
    (call $send (i32.const 0) (i32.const 192) (i32.const 4)))
)
