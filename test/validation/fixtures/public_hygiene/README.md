# Public hygiene scanner fixtures

`test/validation/public_hygiene_test.sh` drives
`tools/validation/check-public-hygiene.sh` with these files.

Each `*.txt` file other than `passing.txt` is one disclosure shape in a
context it could realistically appear in, with `@VALUE@` where the
disclosed value goes. A generated value writes `@NL@` for a line break,
for the shapes whose key and value are on different lines. The file name
selects the class and the values: the test generates one value for every
alternative the shape has (each key
armour, each token prefix, each label number, each escape form) and
fresh random payload characters on every run. It commits every expansion
to a throwaway repository, scans once, and requires each one to be
reported with the file's class and without its value in the output. The
same file with a synthetic word in place of `@VALUE@` must pass, so a
finding comes from the value and not from its context. No file here
carries a real-shaped value itself: a committed key, token or address
would be the disclosure the scanner exists to stop, and would trip secret
scanning on every clone. The test fails on a fixture it has no generator
for, and on a generator without a fixture.

`passing.txt` holds the synthetic and public forms the source policy
accepts, each beside a shape that fails in another fixture. It is scanned
as committed, and must pass.
