;; A test plugin speaking ABI v1 by hand. `ferrule_call` dispatches on the
;; first letter of the tool name:
;;   e  echo the arguments as the output
;;   h  pass the arguments to `ferrule.host_call` as the request, output the reply
;;   l  loop forever (fuel / deadline)
;;   m  grow memory until refused, then trap (the memory cap)
;;   b  reply with a 5 MiB region (the reply cap)
;;   j  reply with bytes that aren't JSON
;;   x  reply {"error": "nope"}
;;   anything else traps
(module
  (import "ferrule" "host_call" (func $host (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (data (i32.const 16) "{\"output\":")
  (data (i32.const 48) "not json")
  (data (i32.const 64) "{\"error\":\"nope\"}")

  (func (export "ferrule_abi_version") (result i32) (i32.const 1))

  (func $alloc (export "ferrule_alloc") (param $n i32) (result i32)
    (local $p i32) (local $end i32)
    (local.set $p (global.get $heap))
    (local.set $end
      (i32.and (i32.add (i32.add (local.get $p) (local.get $n)) (i32.const 7)) (i32.const -8)))
    (block $ok
      (loop $grow
        (br_if $ok (i32.le_u (local.get $end) (i32.shl (memory.size) (i32.const 16))))
        (if (i32.eq (memory.grow (i32.const 1)) (i32.const -1)) (then unreachable))
        (br $grow)))
    (global.set $heap (local.get $end))
    (local.get $p))

  (func $copy (param $dst i32) (param $src i32) (param $n i32)
    (block $done
      (loop $next
        (br_if $done (i32.eqz (local.get $n)))
        (i32.store8 (local.get $dst) (i32.load8_u (local.get $src)))
        (local.set $dst (i32.add (local.get $dst) (i32.const 1)))
        (local.set $src (i32.add (local.get $src) (i32.const 1)))
        (local.set $n (i32.sub (local.get $n) (i32.const 1)))
        (br $next))))

  (func $pack (param $p i32) (param $n i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $n))))

  ;; {"output":<n bytes at p>}
  (func $wrap (param $p i32) (param $n i32) (result i64)
    (local $o i32)
    (local.set $o (call $alloc (i32.add (local.get $n) (i32.const 11))))
    (call $copy (local.get $o) (i32.const 16) (i32.const 10))
    (call $copy (i32.add (local.get $o) (i32.const 10)) (local.get $p) (local.get $n))
    (i32.store8 (i32.add (i32.add (local.get $o) (i32.const 10)) (local.get $n)) (i32.const 125))
    (call $pack (local.get $o) (i32.add (local.get $n) (i32.const 11))))

  (func (export "ferrule_call") (param $tp i32) (param $tl i32) (param $ap i32) (param $al i32) (result i64)
    (local $c i32) (local $r i64)
    (local.set $c (i32.load8_u (local.get $tp)))
    (if (i32.eq (local.get $c) (i32.const 101))
      (then (return (call $wrap (local.get $ap) (local.get $al)))))
    (if (i32.eq (local.get $c) (i32.const 104))
      (then
        (local.set $r (call $host (local.get $ap) (local.get $al)))
        (return (call $wrap (i32.wrap_i64 (i64.shr_u (local.get $r) (i64.const 32)))
                            (i32.wrap_i64 (local.get $r))))))
    (if (i32.eq (local.get $c) (i32.const 108))
      (then (loop $spin (br $spin))))
    (if (i32.eq (local.get $c) (i32.const 109))
      (then
        (loop $more
          (br_if $more (i32.ne (memory.grow (i32.const 16)) (i32.const -1))))
        unreachable))
    (if (i32.eq (local.get $c) (i32.const 98))
      (then (return (i64.const 5242880))))
    (if (i32.eq (local.get $c) (i32.const 106))
      (then (return (call $pack (i32.const 48) (i32.const 8)))))
    (if (i32.eq (local.get $c) (i32.const 120))
      (then (return (call $pack (i32.const 64) (i32.const 16)))))
    unreachable)
)
