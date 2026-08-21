;; ping — sleeps, then opens a conversation with pong.
;; The sleep is the point: it suspends this fiber, not the OS thread.
(module
  (import "strand" "log"      (func $log   (param i32 i32)))
  (import "strand" "sleep_ms" (func $sleep (param i64)))
  (import "strand" "send"     (func $send  (param i32 i32 i32)))

  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 1024))

  (data (i32.const 64)  "ping: starting")
  (data (i32.const 128) "ping: woke, sending PING to pong")
  (data (i32.const 192) "PING")

  (func (export "strand_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))

  (func (export "strand_main")
    (call $log (i32.const 64) (i32.const 14))
    (call $sleep (i64.const 300))
    (call $log (i32.const 128) (i32.const 32))
    (call $send (i32.const 0) (i32.const 192) (i32.const 4)))

  (func (export "strand_on_message") (param $port i32) (param $ptr i32) (param $len i32)
    (call $log (local.get $ptr) (local.get $len)))
)
