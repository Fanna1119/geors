-- wrk script: each request is a random path from the file named by $QUERIES.
local paths = {}
for line in io.lines(os.getenv("QUERIES")) do
  if #line > 0 then paths[#paths + 1] = line end
end
math.randomseed(os.time() + tonumber(tostring({}):sub(8), 16))

request = function()
  return wrk.format("GET", paths[math.random(#paths)])
end
