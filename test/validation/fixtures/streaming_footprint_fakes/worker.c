// SPDX-License-Identifier: Apache-2.0
//
// A stand-in for the serving worker: answers /health the way the real one
// does and holds a /stream connection open. Build-time constants add known
// amounts to the stripped file (PAYLOAD_BYTES of initialized read-only data),
// to the idle resident set (IDLE_MIB touched at start) and to each held
// stream (STREAM_MIB touched per connection), so the harness's deltas can be
// checked against what the fakes were told to add.
//
// With `streaming.enabled` in its config it does what the real worker does:
// built with STREAMING it binds a stream listener on an ephemeral port after
// its HTTP listener and logs the real worker's line for it; built without, it
// refuses the config at start with the real worker's error and exit status.
// NO_STREAM_LISTENER stands for a build with the feature from before the
// listener existed, LISTENER_DELAY_MS delays the line that long past the first
// /health answer, and
// LISTENER_MIB is touched before the line, so samples taken earlier miss it.

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef PAYLOAD_BYTES
#define PAYLOAD_BYTES 16
#endif
#ifndef IDLE_MIB
#define IDLE_MIB 0
#endif
#ifndef STREAM_MIB
#define STREAM_MIB 0
#endif

static const volatile unsigned char payload[PAYLOAD_BYTES] = {1};
static int answered_health;

static void *touch(size_t mib) {
  if (mib == 0) {
    return NULL;
  }
  size_t bytes = mib << 20;
  // Written through a volatile pointer, a byte per page: a compiler that sees
  // malloc, memset and free alone removes all three, and nothing is resident.
  volatile unsigned char *block = malloc(bytes);
  if (block == NULL) {
    abort();
  }
  for (size_t offset = 0; offset < bytes; offset += 1024) {
    block[offset] = 1;
  }
  return (void *)block;
}

static void send_all(int fd, const char *text) {
  size_t left = strlen(text);
  while (left > 0) {
    ssize_t wrote = write(fd, text, left);
    if (wrote <= 0) {
      return;
    }
    text += wrote;
    left -= (size_t)wrote;
  }
}

// The real worker's config is JSON; this reads only `"streaming": {"enabled": true`.
static int streaming_enabled(const char *path) {
  static char text[65536];
  FILE *file = fopen(path, "r");
  if (file == NULL) {
    return 0;
  }
  size_t got = fread(text, 1, sizeof text - 1, file);
  fclose(file);
  text[got] = '\0';
  const char *at = strstr(text, "\"streaming\"");
  at = at == NULL ? NULL : strstr(at, "\"enabled\"");
  if (at == NULL) {
    return 0;
  }
  at += strlen("\"enabled\"");
  while (*at == ' ' || *at == ':' || *at == '\n') {
    ++at;
  }
  return strncmp(at, "true", 4) == 0;
}

#if defined(STREAMING) && !defined(NO_STREAM_LISTENER)
static void *stream_listener(void *arg) {
  const char *host = arg;
#ifdef LISTENER_DELAY_MS
  while (!__atomic_load_n(&answered_health, __ATOMIC_ACQUIRE)) {
    usleep(10000);
  }
  usleep(LISTENER_DELAY_MS * 1000);
#endif
#ifdef LISTENER_MIB
  (void)touch(LISTENER_MIB);  // never freed: it stays resident while the listener does
#endif
  int server = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in address;
  memset(&address, 0, sizeof address);
  address.sin_family = AF_INET;
  inet_pton(AF_INET, host, &address.sin_addr);
  socklen_t length = sizeof address;
  if (bind(server, (struct sockaddr *)&address, sizeof address) != 0 || listen(server, 64) != 0 ||
      getsockname(server, (struct sockaddr *)&address, &length) != 0) {
    perror("fake worker: stream listener");
    exit(1);
  }
  fprintf(stderr,
          "{\"component\":\"serving\",\"fields\":{\"host\":\"%s\",\"port\":%d,"
          "\"serves_sessions\":false},\"level\":\"info\",\"message\":\"stream listener bound\","
          "\"ts_ns\":0}\n",
          host, ntohs(address.sin_port));
  return NULL;
}
#endif

static void *serve(void *arg) {
  int fd = (int)(intptr_t)arg;
  char request[4096];
  ssize_t got = read(fd, request, sizeof request - 1);
  if (got <= 0) {
    close(fd);
    return NULL;
  }
  request[got] = '\0';
  if (strncmp(request, "GET /health", 11) == 0) {
#ifdef NEVER_READY
    const char *state = "degraded";
#else
    const char *state = "ready";
#endif
    char body[256];
    snprintf(body, sizeof body,
             "{\"schema_version\":\"0.1\",\"state\":\"%s\",\"endpoint\":\"default\","
             "\"backend\":\"mock\",\"queue_depth\":0,\"in_flight\":0,\"state_since_steady_ns\":0}",
             state);
    char head[256];
    snprintf(head, sizeof head,
             "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %zu\r\n"
             "Connection: close\r\n\r\n",
             strlen(body));
    send_all(fd, head);
    send_all(fd, body);
    __atomic_store_n(&answered_health, 1, __ATOMIC_RELEASE);
  } else if (strncmp(request, "GET /stream", 11) == 0) {
    void *held = touch(STREAM_MIB);
    // The status line tells the driver the stream's memory is in place.
    send_all(fd, "HTTP/1.1 200 OK\r\n");
    while (read(fd, request, sizeof request) > 0) {
    }
    free(held);
  } else {
    send_all(fd, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
  }
  close(fd);
  return NULL;
}

int main(int argc, char **argv) {
  int port = 0;
  const char *host = "127.0.0.1";
  const char *config = NULL;
  for (int i = 1; i < argc; ++i) {
    if (strcmp(argv[i], "--version") == 0) {
      printf("tensorplate-serving 0.0.0-fake\nprotocol 0.1\nbundle-format 0.1\n");
      return 0;
    }
    if (strcmp(argv[i], "--bind-port") == 0 && i + 1 < argc) {
      port = atoi(argv[++i]);
    } else if (strcmp(argv[i], "--bind-host") == 0 && i + 1 < argc) {
      host = argv[++i];
    } else if (strcmp(argv[i], "--config") == 0 && i + 1 < argc) {
      config = argv[++i];
    }
  }
  int streaming = config != NULL && streaming_enabled(config);
#ifndef STREAMING
  if (streaming) {
    fprintf(stderr,
            "{\"component\":\"serving\",\"fields\":{\"code\":\"unsupported\",\"message\":"
            "\"serving config: streaming.enabled requires TP_ENABLE_STREAMING_GRPC\"},"
            "\"level\":\"error\",\"message\":\"worker startup failed\",\"ts_ns\":0}\n");
    return 64;
  }
#endif
#ifdef EXIT_AT_START
  fprintf(stderr, "fake worker: exiting at start as built\n");
  return 3;
#endif
  void *idle = touch(IDLE_MIB);
  int server = socket(AF_INET, SOCK_STREAM, 0);
  int one = 1;
  setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  struct sockaddr_in address;
  memset(&address, 0, sizeof address);
  address.sin_family = AF_INET;
  address.sin_port = htons((uint16_t)port);
  inet_pton(AF_INET, host, &address.sin_addr);
  if (bind(server, (struct sockaddr *)&address, sizeof address) != 0 || listen(server, 64) != 0) {
    perror("fake worker: bind");
    return 1;
  }
  fprintf(stderr, "fake worker: listening on %s:%d, payload %d bytes, idle %d MiB, %d MiB per stream\n",
          host, port, payload[0] ? PAYLOAD_BYTES : 0, IDLE_MIB, STREAM_MIB);
#if defined(STREAMING) && !defined(NO_STREAM_LISTENER)
  pthread_t listener;
  if (streaming && pthread_create(&listener, NULL, stream_listener, (void *)host) != 0) {
    return 1;
  }
#else
  (void)streaming;
#endif
  for (;;) {
    int fd = accept(server, NULL, NULL);
    if (fd < 0) {
      continue;
    }
    pthread_t thread;
    pthread_attr_t attributes;
    pthread_attr_init(&attributes);
    pthread_attr_setdetachstate(&attributes, PTHREAD_CREATE_DETACHED);
    if (pthread_create(&thread, &attributes, serve, (void *)(intptr_t)fd) != 0) {
      close(fd);
    }
    pthread_attr_destroy(&attributes);
  }
  free(idle);
  return 0;
}
