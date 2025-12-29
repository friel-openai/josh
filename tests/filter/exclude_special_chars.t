  $ export TERM=dumb
  $ export RUST_LOG_STYLE=never

  $ git init -q 1> /dev/null

  $ echo keep > keep.txt
  $ echo a > 'alpha@beta.txt'
  $ echo b > 'gamma+(1).txt'
  $ echo c > 'zeta[inner].txt'
  $ echo d > 'eta]inner.txt'
  $ git add .
  $ git commit -m "add files" 1> /dev/null

  $ josh-filter -s ':exclude[::"alpha@beta.txt",::"gamma+(1).txt",::"zeta[inner].txt",::"eta]inner.txt"]' master --update refs/josh/filtered 1> /dev/null

  $ git ls-tree --name-only -r refs/josh/filtered
  keep.txt
