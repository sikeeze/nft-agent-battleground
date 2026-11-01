// floor_sniper S baca SkillInput ( JSON ) dari stdin, balas SkillOutput.
// Input: { floors: { "<koleksi>": <floor_eth> }, gas_gwei, collections }.
// Output: { message: "...", tool_calls: [] }.
const chunks = [];
process.stdin.on("data", (c) => chunks.push(c));
process.stdin.on("end", () => {
  let input = {};
  try {
    input = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    input = {};
  }
  const floors = input.floors || {};
  const lines = Object.entries(floors).map(([kol, floor]) => {
    return `${kol}: floor ${Number(floor).toFixed(3)} ETH`;
  });
  const message =
    lines.length > 0
      ? `Floor koleksi:\n${lines.join("\n")}`
      : "Tidak ada data floor.";
  console.log(JSON.stringify({ message, tool_calls: [] }));
});
