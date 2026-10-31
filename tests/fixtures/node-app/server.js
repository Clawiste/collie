const port = Number(process.env.PORT);
// A last line on stop, written before exiting: stdout to a pipe is asynchronous here, so exit waits for the write.
process.on("SIGTERM", () => process.stdout.write("shutting down\n", () => process.exit(0)));
require("http")
  .createServer((req, res) => {
    console.log(`GET ${req.url}`);
    res.end("ok\n");
  })
  .listen(port, () => console.log(`listening on ${port}`));
