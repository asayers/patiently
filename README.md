<h1 align="center">Patiently</h1>
<p align="center">A simple parallel job queue</p>

---

Running `patiently $cmd` is exactly the same as running `$cmd`, except that
it will wait its turn before running.

Typically you'd use it like this:

```bash
for x in $(inputs); do
    patiently -j8 "myprog $x" >"$x.json" &
done
wait
```

You can also run `patiently` without any arguments to see a live status report:

```console
$ patiently
   waiting: 632
   running: 8
  finished: 1378
    failed: 0
   crashed: 0
```

## Inspiration

The design of patiently is completely inspired by [nq].  The main differences are:

* nq forks automatically, patiently remains in the foreground (unless you
  background it yourself).
* nq redirects stdout/stderr to a file automatically, patiently passes
  them through to the terminal (unless you redirect them yourself).
* nq doesn't support parallelism (ie. no `-j` flag).

[nq]: https://github.com/leahneukirchen/nq
