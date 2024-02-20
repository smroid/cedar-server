import 'package:cedar_flutter/cedar.pb.dart';
import 'package:fixnum/fixnum.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';
import 'package:settings_ui/settings_ui.dart';
import 'package:sprintf/sprintf.dart';
import 'google/protobuf/duration.pb.dart' as proto_duration;

// Determines if 'prev' and 'curr' have any different fields. Fields that
// are the same are cleared from 'curr'.
bool diffPreferences(Preferences prev, Preferences curr) {
  bool hasDiff = false;
  if (curr.celestialCoordFormat != prev.celestialCoordFormat) {
    hasDiff = true;
  } else {
    curr.clearCelestialCoordFormat();
  }
  if (curr.slewBullseyeSize != prev.slewBullseyeSize) {
    hasDiff = true;
  } else {
    curr.clearSlewBullseyeSize();
  }
  if (curr.nightVisionTheme != prev.nightVisionTheme) {
    hasDiff = true;
  } else {
    curr.clearNightVisionTheme();
  }
  if (curr.showPerfStats != prev.showPerfStats) {
    hasDiff = true;
  } else {
    curr.clearShowPerfStats();
  }
  if (curr.hideAppBar != prev.hideAppBar) {
    hasDiff = true;
  } else {
    curr.clearHideAppBar();
  }
  return hasDiff;
}

// Determines if 'prev' and 'curr' have any different fields. Fields that
// are the same are cleared from 'curr'. Only 'update_interval',
// 'dwell_update_interval', and 'log_dwelled_position' are considered; all other
// fields are cleared in 'curr'.
bool diffOperationSettings(OperationSettings prev, OperationSettings curr) {
  // We don't consider these fields.
  curr.clearOperatingMode();
  curr.clearExposureTime();
  curr.clearAccuracy();

  bool hasDiff = false;
  if (curr.updateInterval != prev.updateInterval) {
    hasDiff = true;
  } else {
    curr.clearUpdateInterval();
  }
  if (curr.dwellUpdateInterval != prev.dwellUpdateInterval) {
    hasDiff = true;
  } else {
    curr.clearDwellUpdateInterval();
  }
  if (curr.logDwelledPositions != prev.logDwelledPositions) {
    hasDiff = true;
  } else {
    curr.clearLogDwelledPositions();
  }
  return hasDiff;
}

class SettingsModel extends ChangeNotifier {
  Preferences preferencesProto = Preferences();
  OperationSettings opSettingsProto = OperationSettings();

  SettingsModel() {
    preferencesProto.slewBullseyeSize = 1.0;
  }

  void updateCelestialCoordFormat(CelestialCoordFormat format) {
    preferencesProto.celestialCoordFormat = format;
    notifyListeners();
  }

  void updateSlewBullseyeSize(double size) {
    preferencesProto.slewBullseyeSize = size;
    notifyListeners();
  }

  void updateNightVisionEnabled(bool enabled) {
    preferencesProto.nightVisionTheme = enabled;
    notifyListeners();
  }

  void updateShowPerfStats(bool enabled) {
    preferencesProto.showPerfStats = enabled;
    notifyListeners();
  }

  void updateHideAppBar(bool hide) {
    preferencesProto.hideAppBar = hide;
    notifyListeners();
  }

  void updateUpdateInterval(int intervalMs) {
    opSettingsProto.updateInterval = _durationFromMs(intervalMs);
    notifyListeners();
  }
}

proto_duration.Duration _durationFromMs(int intervalMs) {
  proto_duration.Duration duration = proto_duration.Duration();
  duration.seconds = Int64(intervalMs ~/ 1000);
  intervalMs -= (duration.seconds * 1000).toInt();
  duration.nanos = intervalMs * 1000000;
  return duration;
}

int _durationToMs(proto_duration.Duration duration) {
  return (duration.seconds * 1000 + duration.nanos ~/ 1000000).toInt();
}

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({super.key});

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  @override
  Widget build(BuildContext context) {
    final provider = Provider.of<SettingsModel>(context, listen: false);
    final prefsProto = provider.preferencesProto;
    final opSettingsProto = provider.opSettingsProto;
    // Need to inset the switches to match the slider.
    const switchInset = 16.0;
    return Scaffold(
        appBar: AppBar(title: const Text('Preferences')),
        body: SettingsList(
            darkTheme: prefsProto.nightVisionTheme
                ? const SettingsThemeData(
                    titleTextColor: Colors.red,
                    settingsTileTextColor: Colors.red,
                    leadingIconsColor: Colors.red)
                : const SettingsThemeData(),
            sections: [
              SettingsSection(title: const Text('Appearance'), tiles: [
                // settings_ui has a bug on Web where the 'trailing' element
                // is not visible. We work around this by putting the important
                // element (the control) in the 'leading' position.
                SettingsTile(
                  leading: Row(children: <Widget>[
                    const SizedBox(width: switchInset, height: 10),
                    Switch(
                        value: prefsProto.hideAppBar,
                        onChanged: (bool value) {
                          setState(() {
                            provider.updateHideAppBar(value);
                          });
                        })
                  ]),
                  title: const Text('Full screen'),
                ),
                SettingsTile(
                  leading: Row(children: <Widget>[
                    const SizedBox(width: switchInset, height: 10),
                    Switch(
                        value: prefsProto.celestialCoordFormat ==
                            CelestialCoordFormat.HMS_DMS,
                        onChanged: (bool value) {
                          setState(() {
                            provider.updateCelestialCoordFormat(value
                                ? CelestialCoordFormat.HMS_DMS
                                : CelestialCoordFormat.DECIMAL);
                          });
                        })
                  ]),
                  title: Text(prefsProto.celestialCoordFormat ==
                          CelestialCoordFormat.HMS_DMS
                      ? 'RA/Dec format H.M.S/D.M.S'
                      : 'RA/Dec format D.DD/D.DD'),
                ),
                SettingsTile(
                  leading: Row(children: <Widget>[
                    const SizedBox(width: switchInset, height: 10),
                    Switch(
                        value: prefsProto.nightVisionTheme,
                        onChanged: (bool value) {
                          setState(() {
                            provider.updateNightVisionEnabled(value);
                          });
                        })
                  ]),
                  title: const Text('Night vision theme'),
                ),
                SettingsTile(
                  leading: Row(children: <Widget>[
                    const SizedBox(width: switchInset, height: 10),
                    Switch(
                        value: prefsProto.showPerfStats,
                        onChanged: (bool value) {
                          setState(() {
                            provider.updateShowPerfStats(value);
                          });
                        })
                  ]),
                  title: const Text('Show performance stats'),
                ),
                SettingsTile(
                  leading: SizedBox(
                      width: 140,
                      height: 40,
                      child: Slider(
                        min: 0.1,
                        max: 2.0,
                        divisions: 19,
                        value: prefsProto.slewBullseyeSize,
                        onChanged: (double value) {
                          setState(() {
                            provider.updateSlewBullseyeSize(value);
                          });
                        },
                      )),
                  title: Text(sprintf(
                      'Telescope FOV  %.1f°', [prefsProto.slewBullseyeSize])),
                ),
              ]),
              SettingsSection(title: const Text('Operation'), tiles: [
                SettingsTile(
                  leading: SizedBox(
                      width: 140,
                      height: 40,
                      child: Slider(
                        // Slider positions represent:
                        // 1000ms (1Hz), 500ms (2Hz), 200ms (5Hz), 100ms (10Hz),
                        // and 0ms (fast as possible).
                        min: 1,
                        max: 5,
                        divisions: 4,
                        value: switch (
                            _durationToMs(opSettingsProto.updateInterval)) {
                          1000 => 1,
                          500 => 2,
                          200 => 3,
                          100 => 4,
                          0 => 5,
                          _ => 5,
                        },
                        onChanged: (double value) {
                          int intervalMs = switch (value.toInt()) {
                            1 => 1000,
                            2 => 500,
                            3 => 200,
                            4 => 100,
                            5 => 0,
                            _ => 0,
                          };
                          setState(() {
                            provider.updateUpdateInterval(intervalMs.round());
                          });
                        },
                      )),
                  title: Text(sprintf('Update frequency %s', [
                    switch (_durationToMs(opSettingsProto.updateInterval)) {
                      1000 => "1Hz",
                      500 => "2Hz",
                      200 => "5Hz",
                      100 => "10Hz",
                      0 => "unlimited",
                      _ => "unknown",
                    },
                  ])),
                ),
              ]),
            ]));
  }
}
