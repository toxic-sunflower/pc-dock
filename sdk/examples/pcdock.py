#!/usr/bin/env python3
"""Маленький клиент pcdockd для примеров: отправляет команды и печатает ответы.

Запуск (демон уже работает на 127.0.0.1:9001):

    python3 pcdock.py '{"op":"List"}'
    python3 pcdock.py '{"op":"DeviceInfo","phone":1}' '{"op":"LaunchApp","phone":1,"package":"com.android.settings"}'

Адрес и пароль: переменные PCDOCK_ADDR (по умолчанию 127.0.0.1:9001) и PCDOCK_TOKEN.
Как вызывать из своей программы: `from pcdock import Conn; c = Conn(); print(c.ask({"op": "List"}))`.
"""
import json
import os
import socket
import struct
import sys


class Conn:
    def __init__(self, addr=None, token=None, timeout=30):
        host, port = (addr or os.environ.get("PCDOCK_ADDR", "127.0.0.1:9001")).rsplit(":", 1)
        self.sock = socket.create_connection((host, int(port)), timeout=timeout)
        self.next_id = 0
        self.hello = self._read_json()  # приветствие сервера
        token = token or os.environ.get("PCDOCK_TOKEN")
        if token:
            self.ask({"op": "Auth", "token": token})

    def _read(self):
        """Один пакет: (тип, тело). Тип 0 это JSON, 1 видео, 2 звук, 3 файл."""
        head = self._exact(4)
        n = struct.unpack("<I", head)[0]
        data = self._exact(n)
        return data[0], data[1:]

    def _exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("демон закрыл соединение")
            buf += chunk
        return buf

    def _read_json(self):
        while True:
            kind, body = self._read()
            if kind == 0:
                return json.loads(body)

    def ask(self, msg):
        """Отправляет команду и ждёт ответ на неё (события других команд пропускает)."""
        self.next_id += 1
        body = b"\x00" + json.dumps(dict(msg, id=self.next_id)).encode()
        self.sock.sendall(struct.pack("<I", len(body)) + body)
        while True:
            reply = self._read_json()
            if reply.get("id") == self.next_id:
                return reply


if __name__ == "__main__":
    conn = Conn()
    for text in sys.argv[1:] or ['{"op":"List"}']:
        print(json.dumps(conn.ask(json.loads(text)), ensure_ascii=False, indent=2))
