;; crasher — §5.4's deliberately-crashable actor.
;;
;; Traps on a message starting with 'B' (BOOM). Otherwise it counts messages
;; in a global, which is the visible proof of arena reclamation: after a
;; restart the count is back to #1 because the Store — and with it the whole
;; arena — was dropped and rebuilt.
(module
  (import "strand" "log" (func $log (param i32 i32)))

  (memory (export "memory") 1)
  (global $bump  (mut i32) (i32.const 1024))
  (global $count (mut i32) (i32.const 0))

  (data (i32.const 64)  "crasher: up")
  (data (i32.const 128) "crasher: handled #1")
  (data (i32.const 192) "crasher: handled again")

  (func (export "strand_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))

  (func (export "strand_main")
    (call $log (i32.const 64) (i32.const 11)))

  (func (export "strand_on_message") (param $port i32) (param $ptr i32) (param $len i32)
    ;; 'B' is 66: the BOOM message kills this actor and nothing else.
    (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 66))
      (then (unreachable)))

    (global.set $count (i32.add (global.get $count) (i32.const 1)))
    (if (i32.eq (global.get $count) (i32.const 1))
      (then (call $log (i32.const 128) (i32.const 19)))
      (else (call $log (i32.const 192) (i32.const 22)))))
)
