#include <stdio.h>
#include <unistd.h>
#include <stdbool.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <linux/vm_sockets.h>

#define PORT 12345

int do_guest_send() {
    int sock;
    int err;
    struct sockaddr_vm addr;
    const char *message = "Hello, world!";

    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    /* We want to connect to the host at PORT */
    addr.svm_port = PORT;
    addr.svm_cid = VMADDR_CID_HOST;

    sock = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (sock < 0) {
        perror("Failed to create socket");
        return -1;
    }

    err = connect(sock, (struct sockaddr *)&addr, sizeof(addr));
    if (err < 0) {
        perror("Failed to connect to host");
        close(sock);
        return -1;
    }

    err = send(sock, message, strlen(message), 0);
    if (err < 0) {
        perror("Failed to send message");
        close(sock);
        return -1;
    }

    close(sock);
    return 0;
}

int do_host_receive() {
    int sock, client_sock;
    int err;
    struct sockaddr_vm addr;
    struct sockaddr_vm client_addr;
    ssize_t bytes_received;
    socklen_t client_addr_len = sizeof(client_addr);
    char buf[1024];

    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    /* We want to allow any client to connect */
    addr.svm_port = PORT;
    addr.svm_cid = VMADDR_CID_ANY;

    sock = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (sock < 0) {
        perror("Failed to create socket");
        return -1;
    }

    err = bind(sock, (struct sockaddr *)&addr, sizeof(addr));
    if (err < 0) {
        perror("Failed to bind socket");
        close(sock);
        return -1;
    }

    err = listen(sock, 1);
    if (err < 0) {
        perror("Failed to listen on socket");
        close(sock);
        return -1;
    }

    client_sock = accept(sock, (struct sockaddr *)&client_addr, &client_addr_len);
    if (client_sock < 0) {
        perror("Failed to accept connection");
        close(sock);
        return -1;
    }

    printf("Accepted connection from client at CID: %u\n", client_addr.svm_cid);
    bytes_received = recv(client_sock, buf, sizeof(buf) - 1, 0);
    if (bytes_received < 0) {
        perror("Failed to receive message");
        close(client_sock);
        close(sock);
        return -1;
    }

    printf("Received message: %.*s\n", (int)bytes_received, buf);
    close(client_sock);
    close(sock);
    return 0;
}

int main(int argc, char *argv[]) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s <guest/host>\n", argv[0]);
        return -1;
    }

    if (argv[1][0] == 'g') {
        return do_guest_send();
    } else if (argv[1][0] == 'h') {
        return do_host_receive();
    } else {
        fprintf(stderr, "Invalid argument: %s\n", argv[1]);
        return -1;
    }
}