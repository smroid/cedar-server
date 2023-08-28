import 'dart:developer';
import 'dart:typed_data';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/widgets.dart' as dart_widgets;
import 'cedar.pbgrpc.dart';
import 'get_cedar_client_for_web.dart'
    if (dart.library.io) 'get_cedar_client.dart';

void main() {
  runApp(const MyApp());
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  // This widget is the root of your application.
  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Cedar',
      theme: ThemeData(
        // This is the theme of your application.
        //
        // TRY THIS: Try running your application with "flutter run". You'll see
        // the application has a blue toolbar. Then, without quitting the app,
        // try changing the seedColor in the colorScheme below to Colors.green
        // and then invoke "hot reload" (save your changes or press the "hot
        // reload" button in a Flutter-supported IDE, or press "r" if you used
        // the command line to start the app).
        //
        // Notice that the counter didn't reset back to zero; the application
        // state is not lost during the reload. To reset the state, use hot
        // restart instead.
        //
        // This works for code too, not just values: Most code changes can be
        // tested with just a hot reload.
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.deepPurple),
        useMaterial3: true,
      ),
      home: const MyHomePage(title: 'Cedar'),
    );
  }
}

class MyHomePage extends StatefulWidget {
  const MyHomePage({super.key, required this.title});

  // This widget is the home page of your application. It is stateful, meaning
  // that it has a State object (defined below) that contains fields that affect
  // how it looks.

  // This class is the configuration for the state. It holds the values (in this
  // case the title) provided by the parent (in this case the App widget) and
  // used by the build method of the State. Fields in a Widget subclass are
  // always marked "final".

  final String title;

  @override
  State<MyHomePage> createState() => _MyHomePageState();
}

class _MyHomePageState extends State<MyHomePage> {
  Uint8List imageBytes = Uint8List(1);
  int width = 0;
  int height = 0;

  Uint8List centerPeakImageBytes = Uint8List(1);
  int centerPeakWidth = 0;
  int centerPeakHeight = 0;

  bool doRefreshes = false;
  int prevFrameId = -1;

  Future<void> getFocusFrameFromServer() async {
    final CedarClient client = getClient();

    final request = FrameRequest()
      ..prevFrameId = prevFrameId
      ..mainImageMode = ImageMode.IMAGE_MODE_BINNED;
    try {
      final response = await client.getFrame(request);
      setState(() {
        prevFrameId = response.frameId;
        if (response.hasImage()) {
          imageBytes = Uint8List.fromList(response.image.imageData);
          width = response.image.rectangle.width;
          height = response.image.rectangle.height;
        }
        centerPeakImageBytes =
            Uint8List.fromList(response.centerPeakImage.imageData);
        centerPeakWidth = response.centerPeakImage.rectangle.width;
        centerPeakHeight = response.centerPeakImage.rectangle.height;
      });
    } catch (e) {
      log('Error: $e');
    }
  }

  void refreshStateFromServer() async {
    await Future.doWhile(() async {
      await getFocusFrameFromServer();
      return doRefreshes;
    });
  }

  @override
  Widget build(BuildContext context) {
    // This method is rerun every time setState is called, for instance as done
    // by the _incrementCounter method above.
    //
    // The Flutter framework has been optimized to make rerunning build methods
    // fast, so that you can just rebuild anything that needs updating rather
    // than having to individually change instances of widgets.
    return Scaffold(
      appBar: AppBar(
        // TRY THIS: Try changing the color here to a specific color (to
        // Colors.amber, perhaps?) and trigger a hot reload to see the AppBar
        // change color while the other colors stay the same.
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        // Here we take the value from the MyHomePage object that was created by
        // the App.build method, and use it to set our appbar title.
        title: Text(widget.title),
      ),
      body: Center(
        // Center is a layout widget. It takes a single child and positions it
        // in the middle of the parent.
        child: Column(
          // Column is also a layout widget. It takes a list of children and
          // arranges them vertically. By default, it sizes itself to fit its
          // children horizontally, and tries to be as tall as its parent.
          //
          // Column has various properties to control how it sizes itself and
          // how it positions its children. Here we use mainAxisAlignment to
          // center the children vertically; the main axis here is the vertical
          // axis because Columns are vertical (the cross axis would be
          // horizontal).
          //
          // TRY THIS: Invoke "debug painting" (choose the "Toggle Debug Paint"
          // action in the IDE, or press "p" in the console), to see the
          // wireframe for each widget.
          mainAxisAlignment: MainAxisAlignment.center,
          children: <Widget>[
            Switch(
                value: doRefreshes,
                onChanged: (bool value) {
                  setState(() {
                    doRefreshes = value;
                    if (doRefreshes) {
                      refreshStateFromServer();
                    }
                  });
                }),
            const SizedBox(height: 2),
            prevFrameId != -1
                ? dart_widgets.Image.memory(centerPeakImageBytes,
                    height: centerPeakHeight.toDouble() * 3,
                    width: centerPeakWidth.toDouble() * 3,
                    fit: BoxFit.fill,
                    gaplessPlayback: true)
                : const SizedBox(height: 2),
            prevFrameId != -1
                ? dart_widgets.Image.memory(imageBytes,
                    height: height.toDouble() / 2,
                    width: width.toDouble() / 2,
                    fit: BoxFit.fill,
                    gaplessPlayback: true)
                : const SizedBox(height: 2),
          ],
        ),
      ),
    );
  }
}
