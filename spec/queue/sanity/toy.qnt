// Backend sanity checks: a counter that may climb to 3 or stutter.
module toy {
  var x: int
  action init = x' = 0
  action inc = all { x < 3, x' = x + 1 }
  action stay = x' = x
  action step = any { inc, stay }
  val inv_true = x <= 3
  val inv_false = x <= 2
  temporal reach3 = eventually(x == 3)
  temporal reach3Fair = weakFair(inc, x) implies eventually(x == 3)
  temporal reach4Fair = weakFair(inc, x) implies eventually(x == 4)
}
