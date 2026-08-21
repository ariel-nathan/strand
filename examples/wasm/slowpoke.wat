;; slowpoke — an actor that takes its time and gets fatter doing it.
;;
;; Exists for §8.4's debug overlay. An actor that returns instantly and never
;; allocates leaves every gauge reading zero, which demonstrates nothing. This
;; one holds its fiber across a sleep — so the mailbox visibly backs up behind
;; it — and grows its arena by a page per message, so `arena` climbs while you
;; watch.
;;
;; The sleep is a host call that suspends the *fiber*, not the thread (§4.4),
;; which is why a second actor keeps running throughout.
(module
  (import "strand" "sleep_ms" (func $sleep (param i64)))

  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 1024))

  (func (export "strand_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))

  (func (export "strand_on_message") (param $port i32) (param $ptr i32) (param $len i32)
    (call $sleep (i64.const 40))
    ;; One more 64K page held for the rest of this life — and handed straight
    ;; back on restart, because the arena goes with the actor (§5.1).
    (drop (memory.grow (i32.const 1))))
)
