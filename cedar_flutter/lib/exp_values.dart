import 'dart:math';

// We allow the user to choose discrete exposure durations:
// 10ms
// 15ms
// 20ms
// 35ms
// 50ms
// 75ms
// 100ms
// 150ms
// and so on, up to the FixedSettings.max_exposure_time value specified by the
// server.
// These values are numbered starting at 1 for 10ms.

int expMsFromIndex(int index) {
  // Make index 0-based.
  --index;
  int decade = index ~/ 6;
  int valIndex = index % 6;

  int value = switch (valIndex) {
    0 => 10,
    1 => 15,
    2 => 20,
    3 => 35,
    4 => 50,
    5 => 75,
    _ => 100, // Should never happen.
  };
  return value * pow(10, decade).toInt();
}

// The argument need not be one of the discrete values returned from
// expMsFromIndex().
int expMsToIndex(int expMs) {
  int decade = 0;
  while (expMs > 75) {
    ++decade;
    expMs ~/= 10;
  }
  int valIndex = switch (expMs) {
    >= 0 && <= 10 => 0,
    >= 11 && <= 15 => 1,
    >= 16 && <= 20 => 2,
    >= 21 && <= 35 => 3,
    >= 36 && <= 50 => 4,
    >= 51 && <= 75 => 5,
    _ => 5, // Should not happen.
  };
  int index = valIndex + decade * 6;
  return index + 1;
}
