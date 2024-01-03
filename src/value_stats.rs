// message ValueStats {
//   // Stats from the most recent 100 results. Omitted if there are no results
//   // yet.
//   optional DescriptiveStats recent = 1;

//   // Stats from the beginning of the session, or since the last session stats
//   // reset (see ActionRequest.reset_session_stats). Omitted if there are
//   // no results since session start or reset.
//   optional DescriptiveStats session = 2;
// }

// // See each item in ProcessingStats for units.
// message DescriptiveStats {
//   double min = 1;
//   double max = 2;

//   double mean = 3;
//   double stddev = 4;

//   // Omitted for `session` stats.
//   optional double median = 5;
//   optional double median_absolute_deviation = 6;
// }

// Use rolling-stats for session stats
// Use medians and statistical for recent stats
