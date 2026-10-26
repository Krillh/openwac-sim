(module
  (func (export "gcd") (param $a i32) (param $b i32) (result i32)
    (local $next i32)
    block $done
      loop $again
        local.get $b
        i32.eqz
        br_if $done
        local.get $a
        local.get $b
        i32.rem_u
        local.set $next
        local.get $b
        local.set $a
        local.get $next
        local.set $b
        br $again
      end
    end
    local.get $a
  )
)
