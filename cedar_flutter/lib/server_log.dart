import 'package:flutter/material.dart';

class ServerLogPopUp extends StatelessWidget {
  final String _content;
  const ServerLogPopUp(this._content, {super.key});

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Row(
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        children: [
          const SizedBox(
              height: 25,
              child: Text('Cedar server log', style: TextStyle(fontSize: 16))),
          SizedBox(
              height: 25,
              child: IconButton(
                  padding: EdgeInsets.zero,
                  icon: const Icon(Icons.close),
                  onPressed: () => Navigator.pop(context))),
        ],
      ),
      content: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(_content, style: const TextStyle(fontSize: 10)),
          ],
        ),
      ),
    );
  }
}
