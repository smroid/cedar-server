import 'package:flutter/material.dart';

class ServerLogPopUp extends StatelessWidget {
  final String _content;
  final ScrollController _scrollController = ScrollController();
  ServerLogPopUp(this._content, {super.key});

  @override
  Widget build(BuildContext context) {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _scrollController.jumpTo(
        _scrollController.position.maxScrollExtent,
      );
    });
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
        controller: _scrollController,
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
