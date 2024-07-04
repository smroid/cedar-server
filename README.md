# cedar-server

Cedar-server integrates several Cedar components to implement a plate-solving
electronic finder for showing where in the sky your telescope is pointed.

Cedar-server:
* Acquires images (cedar-camera)
* Detects and centroids stars (cedar-detect)
* Plate solves (cedar-solve)
* Serves a gRPC endpoint used by clients such as Cedar Aim to present
  a user interface

For more information about Cedar-server, see [about.md](about.md). For
installation and running instructions, see [building.md](building.md).

Please join the [Cedar Discord](<https://discord.gg/JGMk4w2KKX>) server
for discussions around Cedar-server and other related topics.
