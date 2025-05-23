# WebSocket Client for CipherMQ
# This script connects to a WebSocket server, receives messages, and saves them to a JSON file.
# It uses a thread-safe queue to process messages asynchronously.
# Dependencies: websocket, time, queue, threading, json

import websocket
import time
import queue
import threading
import json

# Initialize a thread-safe queue for storing received messages
message_queue = queue.Queue()

def on_message(ws, message):
    """Handle incoming WebSocket messages and add them to the queue.

    Args:
        ws (websocket.WebSocketApp): The WebSocket connection instance.
        message (str): The received message, expected to be JSON or plain text.
    """
    try:
        messages = json.loads(message)  # Parse JSON array of messages
        for msg in messages:
            message_queue.put(msg)
    except json.JSONDecodeError:
        message_queue.put(message)  # Handle single messages for backward compatibility
    print(f"Received: {message}")

def on_error(ws, error):
    """Handle WebSocket errors.

    Args:
        ws (websocket.WebSocketApp): The WebSocket connection instance.
        error (Exception): The error that occurred.
    """
    print(f"Error: {error}")

def on_close(ws, close_status_code, close_msg):
    """Handle WebSocket connection closure.

    Args:
        ws (websocket.WebSocketApp): The WebSocket connection instance.
        close_status_code (int): The status code for the closure.
        close_msg (str): The closure message.
    """
    print(f"Connection closed: {close_status_code}, {close_msg}")

def on_open(ws):
    """Handle WebSocket connection opening.

    Args:
        ws (websocket.WebSocketApp): The WebSocket connection instance.
    """
    print("Connected to WebSocket")

def process_messages():
    """Process messages from the queue and save them to a JSON file."""
    with open("received_messages.json", "a") as f:
        while True:
            try:
                # Retrieve a message from the queue with a 1-second timeout
                message = message_queue.get(timeout=1)
                json.dump({"message": message, "timestamp": time.time()}, f)
                f.write("\n")
                print(f"Processed: {message}")
                message_queue.task_done()
            except queue.Empty:
                continue

if __name__ == "__main__":
    # Start a daemon thread to process messages from the queue
    threading.Thread(target=process_messages, daemon=True).start()
    while True:
        try:
            # Initialize and run the WebSocket client
            ws = websocket.WebSocketApp(
                "ws://127.0.0.1:8080",
                on_open=on_open,
                on_message=on_message,
                on_error=on_error,
                on_close=on_close
            )
            ws.run_forever()
        except Exception as e:
            # Retry connection after 1 second if it fails
            print(f"Connection failed: {e}. Retrying in 1 second...")
            time.sleep(1)
