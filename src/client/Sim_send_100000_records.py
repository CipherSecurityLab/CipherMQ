# Benchmarking Script for Secure Message Broker
# This script sends 100000 messages to the message broker to measure performance metrics
# such as latency, throughput, and success rate. Results are saved to a CSV file.
# Dependencies: aiohttp, asyncio, csv, logging, datetime, tenacity, multiprocessing

import aiohttp
import asyncio
import csv
import time
import logging
from datetime import datetime
from tenacity import retry, stop_after_attempt, wait_fixed
import multiprocessing

# Configure logging for debugging and monitoring
logging.basicConfig(level=logging.INFO, format="%(asctime)s - %(levelname)s - %(message)s")
logger = logging.getLogger(__name__)

# Configuration settings
URL = "http://127.0.0.1:3000/input"  # Endpoint for single message input
BATCH_URL = "http://127.0.0.1:3000/input/batch"  # Endpoint for batch message input
NUM_MESSAGES = 100000  # Total number of messages to send
OUTPUT_FILE = "benchmark_results.csv"  # Output file for benchmark results
#CONCURRENCY = multiprocessing.cpu_count() * 2  # Number of concurrent tasks, based on CPU cores
CONCURRENCY = 1000 # Number of concurrent task, set to high value
BATCH_SIZE = 20000  # Optimal batch size for sending messages

@retry(stop=stop_after_attempt(3), wait=wait_fixed(1))
async def send_message(session, message_id):
    """Send a single message to the server and return latency and status.

    Args:
        session (aiohttp.ClientSession): The HTTP session for sending requests.
        message_id (int): The unique identifier for the message.

    Returns:
        dict: A dictionary containing message_id, status_code, latency, timestamp,
              success status, and error message (if any).
    """
    data = {"content": f"message_{message_id}"}
    start_time = time.time()
    try:
        async with session.post(URL, json=data) as response:
            latency = time.time() - start_time
            if response.status != 200:
                response_text = await response.text()
                logger.error(f"Failed message {message_id}, status: {response.status}, response: {response_text}")
            else:
                logger.info(f"Sent message {message_id}, status: {response.status}")
            return {
                "message_id": message_id,
                "status_code": response.status,
                "latency": latency,
                "timestamp": datetime.now().isoformat(),
                "success": response.status == 200,
                "error": response_text if response.status != 200 else None
            }
    except Exception as e:
        latency = time.time() - start_time
        logger.error(f"Failed to send message {message_id}: {e}")
        return {
            "message_id": message_id,
            "status_code": None,
            "latency": latency,
            "timestamp": datetime.now().isoformat(),
            "success": False,
            "error": str(e)
        }

@retry(stop=stop_after_attempt(3), wait=wait_fixed(1))
async def send_batch(session, messages):
    """Send a batch of messages to the server.

    Args:
        session (aiohttp.ClientSession): The HTTP session for sending requests.
        messages (list): List of message IDs to send in the batch.

    Returns:
        list: A list of dictionaries containing results for each message, including
              message_id, status_code, latency, timestamp, success status, and error
              message (if any).
    """
    data = {
        "messages": [{"content": f"message_{msg_id}"} for msg_id in messages]
    }
    start_time = time.time()
    try:
        async with session.post(BATCH_URL, json=data) as response:
            latency = time.time() - start_time
            if response.status != 200:
                response_text = await response.text()
                logger.error(f"Failed batch of {len(messages)} messages, status: {response.status}, response: {response_text}")
            else:
                logger.info(f"Sent batch of {len(messages)} messages, status: {response.status}")
            return [{
                "message_id": msg_id,
                "status_code": response.status,
                "latency": latency / len(messages),
                "timestamp": datetime.now().isoformat(),
                "success": response.status == 200,
                "error": response_text if response.status != 200 else None
            } for msg_id in messages]
    except Exception as e:
        latency = time.time() - start_time
        logger.error(f"Failed to send batch: {e}")
        return [{
            "message_id": msg_id,
            "status_code": None,
            "latency": latency / len(messages),
            "timestamp": datetime.now().isoformat(),
            "success": False,
            "error": str(e)
        } for msg_id in messages]

async def main():
    """Main function to run the benchmarking process."""
    logger.info(f"Starting benchmark: Sending {NUM_MESSAGES} messages with batch size {BATCH_SIZE}...")
    results = []
    start_time = time.time()

    # Create an HTTP session with a connection limit for concurrency
    async with aiohttp.ClientSession(connector=aiohttp.TCPConnector(limit=100)) as session:
        tasks = []
        message_ids = list(range(1, NUM_MESSAGES + 1))

        # Process messages in batches
        batch_count = 0
        for i in range(0, NUM_MESSAGES, BATCH_SIZE):
            batch_ids = message_ids[i:i + BATCH_SIZE]
            tasks.append(send_batch(session, batch_ids))
            batch_count += 1
            if len(tasks) >= CONCURRENCY:
                # Wait for the current batch of tasks to complete
                batch_results = await asyncio.gather(*tasks, return_exceptions=True)
                for batch in batch_results:
                    if isinstance(batch, list):
                        results.extend(batch)
                tasks = []
                logger.info(f"{len(results)} messages sent ({len(results)/NUM_MESSAGES*100:.1f}%)...")

        # Complete any remaining tasks
        if tasks:
            batch_results = await asyncio.gather(*tasks, return_exceptions=True)
            for batch in batch_results:
                if isinstance(batch, list):
                    results.extend(batch)
            logger.info(f"{len(results)} messages sent (100%)...")

    total_time = time.time() - start_time
    # Calculate performance metrics
    successful_requests = sum(1 for r in results if r["success"])
    avg_latency = (
        sum(r["latency"] for r in results if r["success"]) / successful_requests
        if successful_requests > 0
        else 0
    )
    throughput = successful_requests / total_time if total_time > 0 else 0

    # Save benchmark results to a CSV file
    try:
        with open(OUTPUT_FILE, mode="w", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=["message_id", "status_code", "latency", "timestamp", "success", "error"])
            writer.writeheader()
            for result in results:
                writer.writerow(result)
        logger.info(f"Results saved to {OUTPUT_FILE}")
    except Exception as e:
        logger.error(f"Failed to save results: {e}")

    # Print a summary of benchmark results
    print("\nBenchmark Summary:")
    print(f"Total messages sent: {NUM_MESSAGES}")
    print(f"Successful requests: {successful_requests}")
    print(f"Success rate: {(successful_requests / NUM_MESSAGES) * 100:.2f}%")
    print(f"Total time: {total_time:.2f} seconds")
    print(f"Average latency: {avg_latency:.4f} seconds")
    print(f"Throughput: {throughput:.2f} messages/second")
    print(f"Batches sent: {batch_count}")
    print(f"Results saved to {OUTPUT_FILE}")

if __name__ == "__main__":
    asyncio.run(main())