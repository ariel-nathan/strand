;; transfer — §5.3 ownership transfer, and what happens when you ignore it.
;;
;; Creates a host-owned buffer, sends it to pong, then deliberately touches the
;; handle again. The second use traps: the allocation moved, so the sender has
;; nothing left to read. Data races are unrepresentable, not discouraged.
(module
  (import "strand" "log"           (func $log     (param i32 i32)))
  (import "strand" "buffer_create" (func $bcreate (param i32 i32) (result i32)))
  (import "strand" "buffer_send"   (func $bsend   (param i32 i32)))
  (import "strand" "buffer_len"    (func $blen    (param i32) (result i32)))

  (memory (export "memory") 1)
  (global $bump   (mut i32) (i32.const 1024))
  (global $handle (mut i32) (i32.const 0))

  (data (i32.const 64)  "HELLO")
  (data (i32.const 128) "sender: transferred the buffer")
  (data (i32.const 192) "sender: touching it again")

  (func (export "strand_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))

  (func (export "strand_main")
    (global.set $handle (call $bcreate (i32.const 64) (i32.const 5)))
    (call $bsend (i32.const 1) (global.get $handle))
    (call $log (i32.const 128) (i32.const 30)))

  (func (export "strand_on_message") (param $port i32) (param $ptr i32) (param $len i32)
    (call $log (i32.const 192) (i32.const 25))
    ;; The handle is stale: ownership moved to actor 1. This traps.
    (drop (call $blen (global.get $handle))))
)
