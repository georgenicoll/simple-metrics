# TODO

Requirements not needed yet, recorded so they shape the design of the socket
API when it is built.

- Request all metrics over a specific time period. The caller gives a start and
  an end time and gets every sampled record in that range, oldest first. Only
  what is still held in memory can be returned, so a range that reaches back
  past the retention window returns just the part that is still there.
- Request a subset of metrics over a specific time period. The same, but the
  caller names the metrics it wants and each returned record contains only
  those.
