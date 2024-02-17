import 'package:cedar_flutter/cedar.pb.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';
import 'package:settings_ui/settings_ui.dart';

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
  return hasDiff;
}

class SettingsModel extends ChangeNotifier {
  Preferences preferencesProto = Preferences();

  void updateNightVisionEnabled(bool enabled) {
    preferencesProto.nightVisionTheme = enabled;
    notifyListeners();
  }
}

class SettingsScreen extends StatefulWidget {
  const SettingsScreen({super.key});

  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  @override
  Widget build(BuildContext context) {
    return Scaffold(
        appBar: AppBar(title: const Text('Preferences')),
        body: SettingsList(sections: [
          SettingsSection(title: const Text('General'), tiles: [
            SettingsTile.switchTile(
              title: const Text('Night vision mode'),
              initialValue: Provider.of<SettingsModel>(context, listen: false)
                  .preferencesProto
                  .nightVisionTheme,
              onToggle: (value) {
                setState(() {
                  Provider.of<SettingsModel>(context, listen: false)
                      .updateNightVisionEnabled(value);
                });
              },
            )
          ])
        ]));
  }
}
