// gas_optimizer S baca SkillInput ( JSON ) dari stdin, balas SkillOutput.
// Input: { gas_gwei: <number> }.
// Output: { message: "Gas X gwei -> tier ...", tool_calls: [] }.
const chunks = [];
process.stdin.on("data", (c) => chunks.push(c));
process.stdin.on("end", () => {
  let input = {};
  try {
    input = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    input = {};
  }
  const gas = Number(input.gas_gwei || 0);
  let tier = "normal";
  if (gas > 60) {
    tier = "aggressive";
  } else if (gas > 30) {
    tier = "fast";
  }
  const message = `Gas ${gas.toFixed(1)} gwei -> tier ${tier}`;
  console.log(JSON.stringify({ message, tool_calls: [] }));
});
