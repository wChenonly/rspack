function component () {
	return <div></div>
}

it('should work with preact-refresh', () => {
  const fs = require("fs")
  let map = fs.readFileSync(__filename + ".map", "utf-8")
  expect(map).toContain(STUB)
})
